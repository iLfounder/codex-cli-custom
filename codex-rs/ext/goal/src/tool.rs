use std::sync::Arc;

use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalClearedEvent;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::validate_thread_goal_objective;
use serde::Deserialize;
use serde::Serialize;

use crate::accounting::GoalAccountingState;
use crate::analytics::GoalAnalytics;
use crate::api::ExpectedGoalVersion;
use crate::api::GoalService;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::spec::CLEAR_GOAL_TOOL_NAME;
use crate::spec::CREATE_GOAL_TOOL_NAME;
use crate::spec::GET_GOAL_TOOL_NAME;
use crate::spec::REPLACE_GOAL_TOOL_NAME;
use crate::spec::UPDATE_GOAL_TOOL_NAME;
use crate::spec::create_clear_goal_tool;
use crate::spec::create_create_goal_tool;
use crate::spec::create_get_goal_tool;
use crate::spec::create_replace_goal_tool;
use crate::spec::create_update_goal_tool;

#[derive(Clone)]
pub(crate) struct GoalToolExecutor {
    kind: GoalToolKind,
    thread_id: ThreadId,
    state_db: Arc<codex_state::StateRuntime>,
    accounting_state: Arc<GoalAccountingState>,
    event_emitter: GoalEventEmitter,
    max_goal_token_budget: Option<i64>,
    goal_service: Arc<GoalService>,
}

#[derive(Clone, Copy)]
enum GoalToolKind {
    Get,
    Create,
    Update,
    Clear,
    Replace,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateGoalRequest {
    pub expected_revision: i64,
    pub objective: String,
    pub token_budget: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct UpdateGoalArgs {
    expected_goal_id: String,
    expected_revision: i64,
    status: ThreadGoalStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ClearGoalArgs {
    expected_goal_id: String,
    expected_revision: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ReplaceGoalArgs {
    expected_goal_id: String,
    expected_revision: i64,
    objective: String,
    token_budget: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GoalToolResponse {
    goal: Option<ThreadGoal>,
    goal_id: Option<String>,
    revision: i64,
    remaining_tokens: Option<i64>,
    completion_budget_report: Option<String>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClearGoalToolResponse {
    cleared: bool,
    previous_goal: ThreadGoal,
    previous_goal_id: String,
    previous_revision: i64,
    revision: i64,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplaceGoalToolResponse {
    previous_goal: ThreadGoal,
    previous_goal_id: String,
    previous_revision: i64,
    goal: ThreadGoal,
    goal_id: String,
    revision: i64,
}

#[derive(Clone, Copy)]
enum CompletionBudgetReport {
    Include,
    Omit,
}

impl GoalToolExecutor {
    pub(crate) fn get(
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        _analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        _metrics: GoalMetrics,
        goal_service: Arc<GoalService>,
    ) -> Self {
        Self {
            kind: GoalToolKind::Get,
            thread_id,
            state_db,
            accounting_state,
            event_emitter,
            max_goal_token_budget: None,
            goal_service,
        }
    }

    pub(crate) fn create(
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        _analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        _metrics: GoalMetrics,
        max_goal_token_budget: Option<i64>,
        goal_service: Arc<GoalService>,
    ) -> Self {
        Self {
            kind: GoalToolKind::Create,
            thread_id,
            state_db,
            accounting_state,
            event_emitter,
            max_goal_token_budget,
            goal_service,
        }
    }

    pub(crate) fn update(
        thread_id: ThreadId,
        state_db: Arc<codex_state::StateRuntime>,
        accounting_state: Arc<GoalAccountingState>,
        _analytics: GoalAnalytics,
        event_emitter: GoalEventEmitter,
        _metrics: GoalMetrics,
        goal_service: Arc<GoalService>,
    ) -> Self {
        Self {
            kind: GoalToolKind::Update,
            thread_id,
            state_db,
            accounting_state,
            event_emitter,
            max_goal_token_budget: None,
            goal_service,
        }
    }

    pub(crate) fn clear(&self) -> Self {
        let mut executor = self.clone();
        executor.kind = GoalToolKind::Clear;
        executor
    }

    pub(crate) fn replace(&self) -> Self {
        let mut executor = self.clone();
        executor.kind = GoalToolKind::Replace;
        executor
    }
}

impl ToolExecutor<ToolCall> for GoalToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(match self.kind {
            GoalToolKind::Get => GET_GOAL_TOOL_NAME,
            GoalToolKind::Create => CREATE_GOAL_TOOL_NAME,
            GoalToolKind::Update => UPDATE_GOAL_TOOL_NAME,
            GoalToolKind::Clear => CLEAR_GOAL_TOOL_NAME,
            GoalToolKind::Replace => REPLACE_GOAL_TOOL_NAME,
        })
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            GoalToolKind::Get => create_get_goal_tool(),
            GoalToolKind::Create => create_create_goal_tool(),
            GoalToolKind::Update => create_update_goal_tool(),
            GoalToolKind::Clear => create_clear_goal_tool(),
            GoalToolKind::Replace => create_replace_goal_tool(),
        }
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async move {
            match self.kind {
                GoalToolKind::Get => self.handle_get(invocation).await,
                GoalToolKind::Create => self.handle_create(invocation).await,
                GoalToolKind::Update => self.handle_update(invocation).await,
                GoalToolKind::Clear => self.handle_clear(invocation).await,
                GoalToolKind::Replace => self.handle_replace(invocation).await,
            }
        })
    }
}

impl GoalToolExecutor {
    async fn handle_get(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let _ = invocation.function_arguments()?;
        let state_goal = self
            .state_db
            .thread_goals()
            .get_thread_goal(self.thread_id)
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("failed to read goal: {err}"))
            })?;
        let revision = match state_goal.as_ref() {
            Some(goal) => goal.revision,
            None => self
                .state_db
                .thread_goals()
                .get_thread_goal_revision(self.thread_id)
                .await
                .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?,
        };
        let goal_id = state_goal.as_ref().map(|goal| goal.goal_id.clone());
        goal_response(
            state_goal.map(protocol_goal_from_state),
            goal_id,
            revision,
            CompletionBudgetReport::Omit,
        )
    }

    async fn handle_create(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let mut request: CreateGoalRequest = parse_arguments(invocation.function_arguments()?)?;
        request.objective = request.objective.trim().to_string();
        validate_thread_goal_objective(&request.objective)
            .map_err(FunctionCallError::RespondToModel)?;
        request.token_budget = request.token_budget.or(self.max_goal_token_budget);
        validate_goal_budget(request.token_budget, self.max_goal_token_budget)
            .map_err(FunctionCallError::RespondToModel)?;

        let outcome = self
            .goal_service
            .create_thread_goal_exact(
                self.state_db.as_ref(),
                self.thread_id,
                request.expected_revision,
                request.objective.as_str(),
                ThreadGoalStatus::Active,
                request.token_budget,
                self.max_goal_token_budget,
            )
            .await
            .map_err(goal_service_tool_error)?;
        let version = outcome.version();
        let goal = outcome.goal.clone();
        outcome
            .apply_runtime_effects(self.goal_service.as_ref())
            .await;
        let turn_id = self.accounting_state.current_turn_id();
        self.emit_goal_updated_from_tool_call(&invocation, turn_id, goal.clone());
        goal_response(
            Some(goal),
            Some(version.goal_id),
            version.revision,
            CompletionBudgetReport::Omit,
        )
    }

    async fn handle_update(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: UpdateGoalArgs = parse_arguments(invocation.function_arguments()?)?;
        if !matches!(
            args.status,
            ThreadGoalStatus::Complete | ThreadGoalStatus::Blocked
        ) {
            return Err(FunctionCallError::RespondToModel(
                "update_goal can only mark the existing goal complete or blocked; pause, resume, budget-limited, and usage-limited status changes are controlled by the user or system"
                    .to_string(),
            ));
        }

        let outcome = self
            .goal_service
            .update_thread_goal_status_exact(
                self.state_db.as_ref(),
                self.thread_id,
                &ExpectedGoalVersion {
                    goal_id: args.expected_goal_id,
                    revision: args.expected_revision,
                },
                args.status,
            )
            .await
            .map_err(goal_service_tool_error)?;
        let version = outcome.version();
        let goal = outcome.goal.clone();
        outcome
            .apply_runtime_effects(self.goal_service.as_ref())
            .await;
        let turn_id = self.accounting_state.clear_current_turn_goal();
        self.emit_goal_updated_from_tool_call(&invocation, turn_id, goal.clone());
        goal_response(
            Some(goal),
            Some(version.goal_id),
            version.revision,
            if args.status == ThreadGoalStatus::Complete {
                CompletionBudgetReport::Include
            } else {
                CompletionBudgetReport::Omit
            },
        )
    }

    async fn handle_clear(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: ClearGoalArgs = parse_arguments(invocation.function_arguments()?)?;
        let outcome = self
            .goal_service
            .clear_thread_goal_exact(
                self.state_db.as_ref(),
                self.thread_id,
                &ExpectedGoalVersion {
                    goal_id: args.expected_goal_id,
                    revision: args.expected_revision,
                },
            )
            .await
            .map_err(goal_service_tool_error)?;
        let turn_id = self.accounting_state.clear_current_turn_goal();
        self.event_emitter.thread_goal_cleared(
            invocation.call_id,
            ThreadGoalClearedEvent {
                thread_id: self.thread_id,
                turn_id,
                previous_goal: outcome.previous_goal.clone(),
                revision: outcome.revision,
            },
        );
        let value = serde_json::to_value(ClearGoalToolResponse {
            cleared: true,
            previous_goal: outcome.previous_goal,
            previous_goal_id: outcome.previous_goal_version.goal_id,
            previous_revision: outcome.previous_goal_version.revision,
            revision: outcome.revision,
        })
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
        Ok(Box::new(JsonToolOutput::new(value)))
    }

    async fn handle_replace(
        &self,
        invocation: ToolCall,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: ReplaceGoalArgs = parse_arguments(invocation.function_arguments()?)?;
        let outcome = self
            .goal_service
            .replace_thread_goal_exact(
                self.state_db.as_ref(),
                self.thread_id,
                &ExpectedGoalVersion {
                    goal_id: args.expected_goal_id,
                    revision: args.expected_revision,
                },
                args.objective.as_str(),
                args.token_budget,
                self.max_goal_token_budget,
            )
            .await
            .map_err(goal_service_tool_error)?;
        self.emit_goal_updated_from_tool_call(
            &invocation,
            self.accounting_state.current_turn_id(),
            outcome.goal.clone(),
        );
        let value = serde_json::to_value(ReplaceGoalToolResponse {
            previous_goal: outcome.previous_goal,
            previous_goal_id: outcome.previous_goal_version.goal_id,
            previous_revision: outcome.previous_goal_version.revision,
            goal: outcome.goal,
            goal_id: outcome.goal_version.goal_id,
            revision: outcome.goal_version.revision,
        })
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
        Ok(Box::new(JsonToolOutput::new(value)))
    }

    fn emit_goal_updated_from_tool_call(
        &self,
        invocation: &ToolCall,
        turn_id: Option<String>,
        goal: ThreadGoal,
    ) {
        self.event_emitter
            .thread_goal_updated(invocation.call_id.clone(), turn_id, goal);
    }
}

fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
}

fn goal_service_tool_error(error: crate::api::GoalServiceError) -> FunctionCallError {
    let message = match error {
        crate::api::GoalServiceError::RevisionConflict {
            current_goal_id,
            current_revision,
        } => format!(
            "goal revision conflict; call get_goal to resync (current_goal_id={current_goal_id:?}, current_revision={current_revision})"
        ),
        error => error.to_string(),
    };
    FunctionCallError::RespondToModel(message)
}

pub(crate) fn validate_goal_budget(
    value: Option<i64>,
    max_goal_token_budget: Option<i64>,
) -> Result<(), String> {
    if let Some(value) = value
        && value <= 0
    {
        return Err("goal budgets must be positive when provided".to_string());
    }
    if let Some(value) = value
        && let Some(max_goal_token_budget) = max_goal_token_budget
        && value > max_goal_token_budget
    {
        return Err(format!(
            "goal token budget {value} exceeds the maximum allowed goal token budget of {max_goal_token_budget}"
        ));
    }
    Ok(())
}

fn goal_response(
    goal: Option<ThreadGoal>,
    goal_id: Option<String>,
    revision: i64,
    completion_budget_report: CompletionBudgetReport,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let value = serde_json::to_value(GoalToolResponse::new(
        goal,
        goal_id,
        revision,
        completion_budget_report,
    ))
    .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
    Ok(Box::new(JsonToolOutput::new(value)))
}

impl GoalToolResponse {
    fn new(
        goal: Option<ThreadGoal>,
        goal_id: Option<String>,
        revision: i64,
        report_mode: CompletionBudgetReport,
    ) -> Self {
        let remaining_tokens = goal.as_ref().and_then(|goal| {
            goal.token_budget
                .map(|budget| (budget - goal.tokens_used).max(0))
        });
        let completion_budget_report = match report_mode {
            CompletionBudgetReport::Include => goal
                .as_ref()
                .filter(|goal| goal.status == ThreadGoalStatus::Complete)
                .and_then(completion_budget_report),
            CompletionBudgetReport::Omit => None,
        };
        Self {
            goal,
            goal_id,
            revision,
            remaining_tokens,
            completion_budget_report,
        }
    }
}

pub(crate) async fn fill_empty_thread_preview_if_possible(
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
    goal: &codex_state::ThreadGoal,
) {
    if let Err(err) = state_db
        .set_thread_preview_if_empty(thread_id, goal.objective.as_str())
        .await
    {
        tracing::warn!(
            "failed to set empty thread preview from goal objective for {thread_id}: {err}"
        );
    }
}

pub(crate) fn protocol_goal_from_state(goal: codex_state::ThreadGoal) -> ThreadGoal {
    ThreadGoal {
        thread_id: goal.thread_id,
        goal_id: goal.goal_id,
        revision: goal.revision,
        objective: goal.objective,
        status: protocol_status_from_state(goal.status),
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at.timestamp(),
        updated_at: goal.updated_at.timestamp(),
    }
}

fn protocol_status_from_state(status: codex_state::ThreadGoalStatus) -> ThreadGoalStatus {
    match status {
        codex_state::ThreadGoalStatus::Active => ThreadGoalStatus::Active,
        codex_state::ThreadGoalStatus::Paused => ThreadGoalStatus::Paused,
        codex_state::ThreadGoalStatus::Blocked => ThreadGoalStatus::Blocked,
        codex_state::ThreadGoalStatus::UsageLimited => ThreadGoalStatus::UsageLimited,
        codex_state::ThreadGoalStatus::BudgetLimited => ThreadGoalStatus::BudgetLimited,
        codex_state::ThreadGoalStatus::Complete => ThreadGoalStatus::Complete,
    }
}

pub(crate) fn state_status_from_protocol(
    status: ThreadGoalStatus,
) -> codex_state::ThreadGoalStatus {
    match status {
        ThreadGoalStatus::Active => codex_state::ThreadGoalStatus::Active,
        ThreadGoalStatus::Paused => codex_state::ThreadGoalStatus::Paused,
        ThreadGoalStatus::Blocked => codex_state::ThreadGoalStatus::Blocked,
        ThreadGoalStatus::UsageLimited => codex_state::ThreadGoalStatus::UsageLimited,
        ThreadGoalStatus::BudgetLimited => codex_state::ThreadGoalStatus::BudgetLimited,
        ThreadGoalStatus::Complete => codex_state::ThreadGoalStatus::Complete,
    }
}

fn completion_budget_report(goal: &ThreadGoal) -> Option<String> {
    if goal.token_budget.is_none() && goal.time_used_seconds <= 0 {
        None
    } else {
        Some(
            "Goal achieved. Report final usage from this tool result's structured goal fields. If `goal.tokenBudget` is present, include token usage from `goal.tokensUsed` and `goal.tokenBudget`. If `goal.timeUsedSeconds` is greater than 0, summarize elapsed time in a concise, human-friendly form appropriate to the response language."
                .to_string(),
        )
    }
}
