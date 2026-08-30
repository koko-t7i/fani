use crate::adapters::native::NativeOperations;
use crate::application::command::{Application, CommandOutput, CommandRequest, OutputReporter};
use anyhow::Result;

pub fn execute(request: CommandRequest, output: &dyn OutputReporter) -> Result<CommandOutput> {
    Application::new(NativeOperations::new(output)).execute(request)
}
