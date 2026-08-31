use std::sync::Arc;
use std::sync::Mutex;

use codex_network_proxy::PROXY_ACTIVE_ENV_KEY;
use codex_protocol::error::CodexErr;
use codex_protocol::models::SandboxPermissions;
use codex_tools::UnifiedExecShellMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::PluginExecutableOutput;
use crate::exec_env::create_env;
use crate::exec_env::inject_apply_patch_env;
use crate::exec_env::inject_session_id_env;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tools::runtimes::strip_managed_proxy_env;
use crate::unified_exec::DEFAULT_MAX_OUTPUT_TOKENS;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MAX_YIELD_TIME_MS;
use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::UnifiedExecContext;

use super::SessionTask;
use super::SessionTaskResult;

pub(crate) struct PluginExecutableTask {
    package_root: AbsolutePathBuf,
    executable: AbsolutePathBuf,
    argv: Vec<String>,
    result_tx: Mutex<Option<oneshot::Sender<Result<PluginExecutableOutput, String>>>>,
}

impl PluginExecutableTask {
    pub(crate) fn new(
        package_root: AbsolutePathBuf,
        executable: AbsolutePathBuf,
        argv: Vec<String>,
        result_tx: oneshot::Sender<Result<PluginExecutableOutput, String>>,
    ) -> Self {
        Self {
            package_root,
            executable,
            argv,
            result_tx: Mutex::new(Some(result_tx)),
        }
    }

    fn finish(&self, result: Result<PluginExecutableOutput, String>) {
        if let Some(result_tx) = self
            .result_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = result_tx.send(result);
        }
    }
}

fn plugin_hook_command(command: &[String]) -> String {
    codex_shell_command::parse_command::shlex_join(command)
}

impl SessionTask for PluginExecutableTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.plugin_executable"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        turn_context: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let result = execute_plugin_executable(
            Arc::clone(&session),
            Arc::clone(&turn_context),
            &self.package_root,
            &self.executable,
            &self.argv,
            cancellation_token.clone(),
        )
        .await;
        self.finish(result);
        if cancellation_token.is_cancelled() {
            Err(CodexErr::TurnAborted)
        } else {
            Ok(None)
        }
    }
}

async fn execute_plugin_executable(
    session: Arc<Session>,
    turn_context: Arc<TurnContext>,
    package_root: &AbsolutePathBuf,
    executable: &AbsolutePathBuf,
    argv: &[String],
    cancellation_token: CancellationToken,
) -> Result<PluginExecutableOutput, String> {
    if package_root.as_path().to_str().is_none() {
        return Err("plugin package path is not UTF-8".to_string());
    }
    let executable_text = executable
        .as_path()
        .to_str()
        .ok_or_else(|| "plugin executable path is not UTF-8".to_string())?;
    let turn_environment = turn_context
        .environments
        .local()
        .cloned()
        .ok_or_else(|| "local execution environment is unavailable".to_string())?;
    if turn_environment.environment.is_remote() {
        return Err("plugin executables require a local environment".to_string());
    }
    let shell_type = turn_environment
        .shell
        .as_ref()
        .map(|shell| shell.shell_type)
        .ok_or_else(|| "local execution shell is unavailable".to_string())?;
    let cwd = PathUri::from_abs_path(package_root);
    let mut command = Vec::with_capacity(argv.len() + 1);
    command.push(executable_text.to_string());
    command.extend(argv.iter().cloned());

    let mut env = create_env(
        turn_environment.shell_environment_policy(),
        Some(session.thread_id),
    );
    inject_session_id_env(&mut env, session.session_id());
    inject_apply_patch_env(&mut env, &turn_context.config.features);
    if env.contains_key(PROXY_ACTIVE_ENV_KEY) {
        strip_managed_proxy_env(&mut env);
    }
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context), &cancellation_token)
        .await
        .map_err(|error| error.to_string())?;
    let process_id = session
        .services
        .unified_exec_manager
        .allocate_process_id()
        .await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        step_context,
        Uuid::new_v4().to_string(),
    );
    let request = ExecCommandRequest {
        command: command.clone(),
        shell_type,
        hook_command: plugin_hook_command(&command),
        process_id,
        yield_time_ms: MAX_YIELD_TIME_MS,
        max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
        cwd: cwd.clone(),
        sandbox_cwd: cwd,
        turn_environment,
        shell_mode: UnifiedExecShellMode::Direct,
        network: turn_context.network.clone(),
        tty: false,
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: Some("Run an installed plugin command".to_string()),
        prefix_rule: None,
    };
    let execution = session
        .services
        .unified_exec_manager
        .exec_command(request, &context);
    let output = tokio::select! {
        output = execution => output.map_err(|error| format!("plugin executable failed: {error:?}"))?,
        () = cancellation_token.cancelled() => {
            session.services.unified_exec_manager.terminate_process(process_id).await;
            session.services.unified_exec_manager.release_process_id(process_id).await;
            return Err("plugin executable was cancelled".to_string());
        }
    };
    let timed_out = output.process_id.is_some();
    if let Some(process_id) = output.process_id
        && !session
            .services
            .unified_exec_manager
            .terminate_process(process_id)
            .await
    {
        return Err("failed to terminate plugin executable at timeout".to_string());
    }
    if output.raw_output.len() > UNIFIED_EXEC_OUTPUT_MAX_BYTES {
        return Err(format!(
            "plugin executable output exceeds {UNIFIED_EXEC_OUTPUT_MAX_BYTES} bytes"
        ));
    }
    let output_text = String::from_utf8(output.raw_output)
        .map_err(|_| "plugin executable output is not UTF-8".to_string())?;
    Ok(PluginExecutableOutput {
        exit_code: output.exit_code,
        output: output_text,
        timed_out,
    })
}

#[cfg(test)]
#[path = "plugin_executable_tests.rs"]
mod tests;
