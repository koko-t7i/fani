use anyhow::Result;
use std::path::PathBuf;

pub const ERROR_EXIT_CODE: i32 = 2;
pub const REASONING_EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Clone, Debug)]
pub struct Selection {
    pub config: PathBuf,
    pub repository: Option<String>,
    pub language: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InitRequest {
    pub config: PathBuf,
    pub repository: PathBuf,
    pub language: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub force: bool,
}

#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub selection: Selection,
    pub report_dir: Option<PathBuf>,
    pub quiet: bool,
}

#[derive(Clone, Debug)]
pub enum ReconcileMode {
    Adopt,
    Discard,
}

#[derive(Clone, Debug)]
pub enum CommandRequest {
    Init(InitRequest),
    Sync(SyncRequest),
    Status(Selection),
    Check(Selection),
    Doctor {
        config: PathBuf,
    },
    Reconcile {
        selection: Selection,
        mode: ReconcileMode,
    },
}

#[derive(Debug, Default)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

pub trait OutputReporter: Send + Sync {
    fn stdout(&self, line: &str);
    fn stderr(&self, line: &str);
}

pub trait CommandOperations {
    fn init(&self, request: InitRequest) -> Result<CommandOutput>;
    fn sync(&self, request: SyncRequest) -> Result<CommandOutput>;
    fn status(&self, request: Selection) -> Result<CommandOutput>;
    fn doctor(&self, config: PathBuf) -> Result<CommandOutput>;
    fn reconcile(&self, selection: Selection, mode: ReconcileMode) -> Result<CommandOutput>;
}

pub struct Application<O> {
    operations: O,
}

impl<O> Application<O>
where
    O: CommandOperations,
{
    pub fn new(operations: O) -> Self {
        Self { operations }
    }

    pub fn execute(&self, request: CommandRequest) -> Result<CommandOutput> {
        match request {
            CommandRequest::Init(request) => self.operations.init(request),
            CommandRequest::Sync(request) => self.operations.sync(request),
            CommandRequest::Status(request) | CommandRequest::Check(request) => {
                self.operations.status(request)
            }
            CommandRequest::Doctor { config } => self.operations.doctor(config),
            CommandRequest::Reconcile { selection, mode } => {
                self.operations.reconcile(selection, mode)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeOperations {
        calls: RefCell<Vec<&'static str>>,
    }

    impl CommandOperations for FakeOperations {
        fn init(&self, _request: InitRequest) -> Result<CommandOutput> {
            self.calls.borrow_mut().push("init");
            Ok(CommandOutput::default())
        }

        fn sync(&self, _request: SyncRequest) -> Result<CommandOutput> {
            self.calls.borrow_mut().push("sync");
            Ok(CommandOutput::default())
        }

        fn status(&self, _request: Selection) -> Result<CommandOutput> {
            self.calls.borrow_mut().push("status");
            Ok(CommandOutput::default())
        }

        fn doctor(&self, _config: PathBuf) -> Result<CommandOutput> {
            self.calls.borrow_mut().push("doctor");
            Ok(CommandOutput::default())
        }

        fn reconcile(&self, _selection: Selection, _mode: ReconcileMode) -> Result<CommandOutput> {
            self.calls.borrow_mut().push("reconcile");
            Ok(CommandOutput::default())
        }
    }

    fn selection() -> Selection {
        Selection {
            config: "fani.toml".into(),
            repository: None,
            language: None,
        }
    }

    #[test]
    fn dispatches_typed_requests_through_the_application_owned_port() {
        let operations = FakeOperations {
            calls: RefCell::new(Vec::new()),
        };
        let application = Application::new(operations);

        application
            .execute(CommandRequest::Init(InitRequest {
                config: "fani.toml".into(),
                repository: ".".into(),
                language: "zh-CN".into(),
                provider: "anthropic".into(),
                model: "test-model".into(),
                reasoning_effort: None,
                force: false,
            }))
            .unwrap();
        application
            .execute(CommandRequest::Status(selection()))
            .unwrap();
        application
            .execute(CommandRequest::Doctor {
                config: "fani.toml".into(),
            })
            .unwrap();
        application
            .execute(CommandRequest::Reconcile {
                selection: selection(),
                mode: ReconcileMode::Adopt,
            })
            .unwrap();

        assert_eq!(
            application.operations.calls.into_inner(),
            ["init", "status", "doctor", "reconcile"]
        );
    }
}
