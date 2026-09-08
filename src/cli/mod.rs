use crate::VERSION;
use crate::application::command::{
    CommandOutput, CommandRequest, ERROR_EXIT_CODE, InitRequest, OutputReporter, REASONING_EFFORTS,
    ReconcileMode, Selection, SyncRequest,
};
use clap::{Args, Parser, Subcommand};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "fani", version = VERSION, about = "Native continuous Markdown and JSON translation for Git and GitHub")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a safe starter configuration for a built-in provider.
    Init(InitArgs),
    /// Translate, verify, recover, materialize, and optionally publish pending work.
    Sync(SyncArgs),
    /// Plan from an immutable Git revision without calling an Agent.
    Status(CommonArgs),
    /// CI-friendly alias for the read-only planning/check path.
    Check(CommonArgs),
    /// Validate configuration and runtime prerequisites.
    Doctor(DoctorArgs),
    /// Validate and adopt divergent human target files.
    Adopt(CommonArgs),
    /// Restore canonical verified target files over divergent human edits.
    Discard(CommonArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(long, default_value = "fani.toml")]
    config: PathBuf,
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    lang: String,
    #[arg(long, default_value = "anthropic")]
    provider: String,
    #[arg(long)]
    model: String,
    #[arg(long, value_parser = REASONING_EFFORTS)]
    reasoning_effort: Option<String>,
    #[arg(long)]
    force: bool,
}

#[derive(Clone, Debug, Args)]
struct CommonArgs {
    #[arg(long, default_value = "fani.toml")]
    config: PathBuf,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    lang: Option<String>,
}

impl From<CommonArgs> for Selection {
    fn from(args: CommonArgs) -> Self {
        Self {
            config: args.config,
            repository: args.repo,
            language: args.lang,
        }
    }
}

#[derive(Debug, Args)]
struct SyncArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long)]
    report_dir: Option<PathBuf>,
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long, default_value = "fani.toml")]
    config: PathBuf,
}

pub(crate) struct TerminalReporter;

impl OutputReporter for TerminalReporter {
    fn stdout(&self, line: &str) {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{line}").expect("cannot write stdout");
        stdout.flush().expect("cannot flush stdout");
    }

    fn stderr(&self, line: &str) {
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "{line}").expect("cannot write stderr");
        stderr.flush().expect("cannot flush stderr");
    }
}

pub fn run() -> i32 {
    let request = match Cli::parse().command {
        Command::Init(args) => CommandRequest::Init(InitRequest {
            config: args.config,
            repository: args.repo,
            language: args.lang,
            provider: args.provider,
            model: args.model,
            reasoning_effort: args.reasoning_effort,
            force: args.force,
        }),
        Command::Sync(args) => CommandRequest::Sync(SyncRequest {
            selection: args.common.into(),
            report_dir: args.report_dir,
            quiet: args.quiet,
        }),
        Command::Status(args) => CommandRequest::Status(args.into()),
        Command::Check(args) => CommandRequest::Check(args.into()),
        Command::Doctor(args) => CommandRequest::Doctor {
            config: args.config,
        },
        Command::Adopt(args) => CommandRequest::Reconcile {
            selection: args.into(),
            mode: ReconcileMode::Adopt,
        },
        Command::Discard(args) => CommandRequest::Reconcile {
            selection: args.into(),
            mode: ReconcileMode::Discard,
        },
    };

    let reporter = TerminalReporter;
    match crate::composition::execute(request, &reporter) {
        Ok(output) => present(output),
        Err(error) => {
            eprintln!("fani: {error:#}");
            ERROR_EXIT_CODE
        }
    }
}

fn present(output: CommandOutput) -> i32 {
    for line in output.stdout {
        println!("{line}");
    }
    for line in output.stderr {
        eprintln!("{line}");
    }
    output.exit_code
}
