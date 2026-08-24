use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::validate_thread_goal_objective;

use crate::api::ExpectedGoalVersion;
use crate::api::GoalClearOutcome;
use crate::api::GoalReplaceOutcome;
use crate::api::GoalService;
use crate::api::GoalServiceError;
use crate::api::GoalSetOutcome;
use crate::runtime::PreviousGoalSnapshot;
use crate::tool::fill_empty_thread_preview_if_possible;
use crate::tool::protocol_goal_from_state;
use crate::tool::state_status_from_protocol;
use crate::tool::validate_goal_budget;

impl GoalService {
    pub async fn set_thread_goal_exact(
        &self,
        state_db: &codex_state::StateRuntime,
        request: crate::api::GoalSetRequest<'_>,
        expected: &ExpectedGoalVersion,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let crate::api::GoalSetRequest {
            thread_id,
            objective,
            status,
            token_budget,
            max_goal_token_budget,
        } = request;
        let objective = match objective {
            crate::api::GoalObjectiveUpdate::Keep => None,
            crate::api::GoalObjectiveUpdate::Set(objective) => {
                let objective = objective.trim();
                validate_thread_goal_objective(objective)
                    .map_err(GoalServiceError::InvalidRequest)?;
                Some(objective.to_string())
            }
        };
        let token_budget = match token_budget {
            crate::api::GoalTokenBudgetUpdate::Keep => None,
            crate::api::GoalTokenBudgetUpdate::Set(token_budget) => {
                let token_budget = token_budget.or(max_goal_token_budget);
                validate_goal_budget(token_budget, max_goal_token_budget)
                    .map_err(GoalServiceError::InvalidRequest)?;
                Some(token_budget)
            }
        };
        let runtime = self.runtime_for_thread(thread_id);
        let goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let current = require_expected_goal(state_db, thread_id, expected).await?;
        let promoted = match runtime.as_ref() {
            Some(runtime) => match runtime
                .prepare_external_goal_mutation_while_goal_state_locked()
                .await
                .map_err(GoalServiceError::Internal)?
            {
                Some(version) => version,
                None => return Err(current_goal_revision_conflict(state_db, thread_id).await),
            },
            None => codex_state::GoalVersion::from(&current),
        };
        let Some(goal) = state_db
            .thread_goals()
            .update_thread_goal_exact(
                thread_id,
                &promoted,
                codex_state::GoalUpdate {
                    objective,
                    status: status.map(state_status_from_protocol),
                    token_budget,
                    expected_goal_id: Some(promoted.goal_id.clone()),
                },
            )
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?
        else {
            return Err(current_goal_revision_conflict(state_db, thread_id).await);
        };
        let previous_goal = PreviousGoalSnapshot::from(&current);
        fill_empty_thread_preview_if_possible(state_db, thread_id, &goal).await;
        drop(goal_state_permit);
        Ok(GoalSetOutcome {
            goal: protocol_goal_from_state(goal.clone()),
            state_goal: goal,
            previous_goal: Some(previous_goal),
        })
    }

    pub async fn create_thread_goal_exact(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
        expected_revision: i64,
        objective: &str,
        status: ThreadGoalStatus,
        token_budget: Option<i64>,
        max_goal_token_budget: Option<i64>,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let objective = objective.trim();
        validate_thread_goal_objective(objective).map_err(GoalServiceError::InvalidRequest)?;
        let token_budget = token_budget.or(max_goal_token_budget);
        validate_goal_budget(token_budget, max_goal_token_budget)
            .map_err(GoalServiceError::InvalidRequest)?;
        let runtime = self.runtime_for_thread(thread_id);
        let goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let current = state_db
            .thread_goals()
            .get_thread_goal(thread_id)
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?;
        let current_revision = current.as_ref().map_or(
            state_db
                .thread_goals()
                .get_thread_goal_revision(thread_id)
                .await
                .map_err(|err| GoalServiceError::Internal(err.to_string()))?,
            |goal| goal.revision,
        );
        if current_revision != expected_revision {
            return Err(goal_revision_conflict(current, current_revision));
        }
        if let Some(goal) = current.as_ref()
            && goal.status != codex_state::ThreadGoalStatus::Complete
        {
            return Err(GoalServiceError::InvalidRequest(
                "cannot create a new goal because this thread has an unfinished goal; complete the existing goal first"
                    .to_string(),
            ));
        }
        let _ = match runtime.as_ref() {
            Some(runtime) => runtime
                .prepare_external_goal_mutation_while_goal_state_locked()
                .await
                .map_err(GoalServiceError::Internal)?,
            None => current.as_ref().map(codex_state::GoalVersion::from),
        };
        let (goal, previous_goal) = match current {
            Some(previous) => {
                let Some(promoted) = state_db
                    .thread_goals()
                    .get_thread_goal(thread_id)
                    .await
                    .map_err(|err| GoalServiceError::Internal(err.to_string()))?
                else {
                    return Err(current_goal_revision_conflict(state_db, thread_id).await);
                };
                let Some(outcome) = state_db
                    .thread_goals()
                    .replace_thread_goal_exact(
                        thread_id,
                        &codex_state::GoalVersion::from(&promoted),
                        objective,
                        state_status_from_protocol(status),
                        token_budget,
                    )
                    .await
                    .map_err(|err| GoalServiceError::Internal(err.to_string()))?
                else {
                    return Err(current_goal_revision_conflict(state_db, thread_id).await);
                };
                (outcome.goal, Some(PreviousGoalSnapshot::from(&previous)))
            }
            None => {
                let Some(goal) = state_db
                    .thread_goals()
                    .create_thread_goal_exact(
                        thread_id,
                        expected_revision,
                        objective,
                        state_status_from_protocol(status),
                        token_budget,
                    )
                    .await
                    .map_err(|err| GoalServiceError::Internal(err.to_string()))?
                else {
                    return Err(current_goal_revision_conflict(state_db, thread_id).await);
                };
                (goal, None)
            }
        };
        fill_empty_thread_preview_if_possible(state_db, thread_id, &goal).await;
        drop(goal_state_permit);
        Ok(GoalSetOutcome {
            goal: protocol_goal_from_state(goal.clone()),
            state_goal: goal,
            previous_goal,
        })
    }

    pub async fn replace_thread_goal_exact(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
        expected: &ExpectedGoalVersion,
        objective: &str,
        token_budget: Option<i64>,
        max_goal_token_budget: Option<i64>,
    ) -> Result<GoalReplaceOutcome, GoalServiceError> {
        let objective = objective.trim();
        validate_thread_goal_objective(objective).map_err(GoalServiceError::InvalidRequest)?;
        let token_budget = token_budget.or(max_goal_token_budget);
        validate_goal_budget(token_budget, max_goal_token_budget)
            .map_err(GoalServiceError::InvalidRequest)?;
        let runtime = self.runtime_for_thread(thread_id);
        let goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let current = require_expected_goal(state_db, thread_id, expected).await?;
        let promoted = match runtime.as_ref() {
            Some(runtime) => match runtime
                .prepare_external_goal_mutation_while_goal_state_locked()
                .await
                .map_err(GoalServiceError::Internal)?
            {
                Some(version) => version,
                None => return Err(current_goal_revision_conflict(state_db, thread_id).await),
            },
            None => codex_state::GoalVersion::from(&current),
        };
        let Some(outcome) = state_db
            .thread_goals()
            .replace_thread_goal_exact(
                thread_id,
                &promoted,
                objective,
                codex_state::ThreadGoalStatus::Active,
                token_budget,
            )
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?
        else {
            return Err(current_goal_revision_conflict(state_db, thread_id).await);
        };
        fill_empty_thread_preview_if_possible(state_db, thread_id, &outcome.goal).await;
        drop(goal_state_permit);
        let result = GoalReplaceOutcome {
            previous_goal: protocol_goal_from_state(outcome.previous_goal.clone()),
            previous_goal_version: ExpectedGoalVersion::from(&outcome.previous_goal),
            goal: protocol_goal_from_state(outcome.goal.clone()),
            goal_version: ExpectedGoalVersion::from(&outcome.goal),
        };
        if let Some(runtime) = runtime
            && let Err(err) = runtime
                .apply_external_goal_set(
                    outcome.goal,
                    Some(PreviousGoalSnapshot::from(&outcome.previous_goal)),
                )
                .await
        {
            tracing::warn!("failed to apply replacement goal runtime effects: {err}");
        }
        Ok(result)
    }

    pub async fn update_thread_goal_status_exact(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
        expected: &ExpectedGoalVersion,
        status: ThreadGoalStatus,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let runtime = self.runtime_for_thread(thread_id);
        let goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let current = require_expected_goal(state_db, thread_id, expected).await?;
        let promoted = match runtime.as_ref() {
            Some(runtime) => match runtime
                .prepare_external_goal_mutation_while_goal_state_locked()
                .await
                .map_err(GoalServiceError::Internal)?
            {
                Some(version) => version,
                None => return Err(current_goal_revision_conflict(state_db, thread_id).await),
            },
            None => codex_state::GoalVersion::from(&current),
        };
        let Some(goal) = state_db
            .thread_goals()
            .update_thread_goal_exact(
                thread_id,
                &promoted,
                codex_state::GoalUpdate {
                    objective: None,
                    status: Some(state_status_from_protocol(status)),
                    token_budget: None,
                    expected_goal_id: Some(promoted.goal_id.clone()),
                },
            )
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?
        else {
            return Err(current_goal_revision_conflict(state_db, thread_id).await);
        };
        let previous_goal = PreviousGoalSnapshot::from(&current);
        drop(goal_state_permit);
        Ok(GoalSetOutcome {
            goal: protocol_goal_from_state(goal.clone()),
            state_goal: goal,
            previous_goal: Some(previous_goal),
        })
    }

    pub async fn clear_thread_goal_exact(
        &self,
        state_db: &codex_state::StateRuntime,
        thread_id: ThreadId,
        expected: &ExpectedGoalVersion,
    ) -> Result<GoalClearOutcome, GoalServiceError> {
        let runtime = self.runtime_for_thread(thread_id);
        let goal_state_permit = match runtime.as_ref() {
            Some(runtime) => Some(
                runtime
                    .goal_state_permit()
                    .await
                    .map_err(GoalServiceError::Internal)?,
            ),
            None => None,
        };
        let current = require_expected_goal(state_db, thread_id, expected).await?;
        let promoted = match runtime.as_ref() {
            Some(runtime) => match runtime
                .prepare_external_goal_mutation_while_goal_state_locked()
                .await
                .map_err(GoalServiceError::Internal)?
            {
                Some(version) => version,
                None => return Err(current_goal_revision_conflict(state_db, thread_id).await),
            },
            None => codex_state::GoalVersion::from(&current),
        };
        let Some(outcome) = state_db
            .thread_goals()
            .clear_thread_goal_exact(thread_id, &promoted)
            .await
            .map_err(|err| GoalServiceError::Internal(err.to_string()))?
        else {
            return Err(current_goal_revision_conflict(state_db, thread_id).await);
        };
        drop(goal_state_permit);
        if let Some(runtime) = runtime
            && let Err(err) = runtime
                .apply_external_goal_clear(outcome.previous_goal.clone())
                .await
        {
            tracing::warn!("failed to apply goal clear runtime effects: {err}");
        }
        Ok(GoalClearOutcome {
            previous_goal_version: ExpectedGoalVersion::from(&outcome.previous_goal),
            previous_goal: protocol_goal_from_state(outcome.previous_goal),
            revision: outcome.revision,
        })
    }
}

async fn require_expected_goal(
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
    expected: &ExpectedGoalVersion,
) -> Result<codex_state::ThreadGoal, GoalServiceError> {
    let goal = state_db
        .thread_goals()
        .get_thread_goal(thread_id)
        .await
        .map_err(|err| GoalServiceError::Internal(err.to_string()))?;
    match goal {
        Some(goal) if ExpectedGoalVersion::from(&goal) == *expected => Ok(goal),
        goal => {
            let revision = match goal.as_ref() {
                Some(goal) => goal.revision,
                None => state_db
                    .thread_goals()
                    .get_thread_goal_revision(thread_id)
                    .await
                    .map_err(|err| GoalServiceError::Internal(err.to_string()))?,
            };
            Err(goal_revision_conflict(goal, revision))
        }
    }
}

fn goal_revision_conflict(
    goal: Option<codex_state::ThreadGoal>,
    current_revision: i64,
) -> GoalServiceError {
    GoalServiceError::RevisionConflict {
        current_goal_id: goal.map(|goal| goal.goal_id),
        current_revision,
    }
}

async fn current_goal_revision_conflict(
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
) -> GoalServiceError {
    let goal = match state_db.thread_goals().get_thread_goal(thread_id).await {
        Ok(goal) => goal,
        Err(err) => return GoalServiceError::Internal(err.to_string()),
    };
    let revision = match goal.as_ref() {
        Some(goal) => goal.revision,
        None => match state_db
            .thread_goals()
            .get_thread_goal_revision(thread_id)
            .await
        {
            Ok(revision) => revision,
            Err(err) => return GoalServiceError::Internal(err.to_string()),
        },
    };
    goal_revision_conflict(goal, revision)
}
