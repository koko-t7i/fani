use crate::adapters::native::NativeOperations;
use crate::application::command::{Application, CommandOutput, CommandRequest, OutputReporter};
use anyhow::Result;
use std::time::Instant;

pub fn execute(request: CommandRequest, output: &dyn OutputReporter) -> Result<CommandOutput> {
    crate::diagnostics::init();
    let command = match &request {
        CommandRequest::Init(_) => "init",
        CommandRequest::Sync(_) => "sync",
        CommandRequest::Status(_) => "status",
        CommandRequest::Check(_) => "check",
        CommandRequest::Doctor { .. } => "doctor",
        CommandRequest::Reconcile { mode, .. } => match mode {
            crate::application::command::ReconcileMode::Adopt => "adopt",
            crate::application::command::ReconcileMode::Discard => "discard",
        },
    };
    let started = Instant::now();
    tracing::info!(event = "cli.command.started", command);
    let result = Application::new(NativeOperations::new(output)).execute(request);
    match &result {
        Ok(command_output) => tracing::info!(
            event = "cli.command.completed",
            command,
            status = "completed",
            exit_code = command_output.exit_code,
            duration_ms = started.elapsed().as_millis() as u64,
        ),
        Err(_) => tracing::error!(
            event = "cli.command.completed",
            command,
            status = "error",
            exit_code = crate::application::command::ERROR_EXIT_CODE,
            duration_ms = started.elapsed().as_millis() as u64,
        ),
    }
    result
}
