use crate::adapters::agent::{RoutedAgentExecutor, executable_on_path};
use crate::adapters::config::Config;
use crate::adapters::db::Database;
use crate::adapters::documentation::NativeDocumentationChecker;
use crate::adapters::github::GithubCodeHost;
use crate::adapters::gitout::NativeGitPublisher;
use crate::adapters::lock::RepoLock;
use crate::adapters::materialize::FilesystemMaterializer;
use crate::adapters::process::current_process_identity;
use crate::adapters::report;
use crate::application::command::{
    CommandOperations, CommandOutput, OutputReporter, ReconcileMode, Selection, SyncRequest,
};
use crate::application::settings::RepoConfig;
use crate::application::sync::{Orchestrator, adopt_human_edit, discard_human_edit};
use crate::domain::model::{LanguageOutcome, Status};
use anyhow::{Result, anyhow};
use chrono::Local;
use std::path::{Path, PathBuf};

pub struct NativeOperations<'a> {
    output: &'a dyn OutputReporter,
}

impl<'a> NativeOperations<'a> {
    pub fn new(output: &'a dyn OutputReporter) -> Self {
        Self { output }
    }
}

fn command_output(exit_code: i32) -> CommandOutput {
    CommandOutput {
        exit_code,
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

fn selected<'a>(config: &'a Config, filter: Option<&str>) -> Result<Vec<&'a RepoConfig>> {
    let Some(filter) = filter else {
        return Ok(config.repos.iter().collect());
    };
    let selected: Vec<_> = config
        .repos
        .iter()
        .filter(|repo| {
            repo.path.file_name().and_then(|name| name.to_str()) == Some(filter)
                || repo.path.to_string_lossy() == filter
        })
        .collect();
    if selected.is_empty() {
        return Err(anyhow!("no configured repository matches {filter:?}"));
    }
    Ok(selected)
}

fn languages(repo: &RepoConfig, filter: Option<&str>) -> Result<Vec<String>> {
    let Some(filter) = filter else {
        return Ok(repo.languages.clone());
    };
    if repo.languages.iter().any(|language| language == filter) {
        Ok(vec![filter.into()])
    } else {
        Err(anyhow!(
            "{} does not configure language {filter:?}",
            repo.path.display()
        ))
    }
}

fn database_path(repo: &RepoConfig) -> PathBuf {
    repo.path.join(&repo.data_dir).join("fani.db")
}

fn repository_trace_id(repo: &RepoConfig) -> String {
    crate::diagnostics::safe_id(repo.path.to_string_lossy().as_bytes())
}

fn open_database(repo: &RepoConfig) -> Result<Database> {
    Database::open(database_path(repo))
}

fn owner_identity() -> Result<String> {
    let (pid, started_at) = current_process_identity()?;
    Ok(format!("{pid}:{started_at}"))
}

fn status(args: Selection, output: &dyn OutputReporter) -> Result<CommandOutput> {
    let config = Config::load(&args.config)?;
    config.check_environment()?;
    let mut worst = Status::Ok;
    for repo in selected(&config, args.repository.as_deref())? {
        let snapshot_dir = tempfile::tempdir()?;
        let snapshot_path = snapshot_dir.path().join("fani.db");
        let authoritative_path = database_path(repo);
        if authoritative_path.exists() {
            Database::snapshot(&authoritative_path, &snapshot_path)?;
        }
        let database = Database::open(&snapshot_path)?;
        let materializer = FilesystemMaterializer;
        let agents = RoutedAgentExecutor::new(&config);
        let documentation = NativeDocumentationChecker;
        let git = NativeGitPublisher;
        let code_host = GithubCodeHost;
        let owner_identity = owner_identity()?;
        let orchestrator = Orchestrator::new(
            repo,
            &database,
            &materializer,
            &agents,
            &documentation,
            &git,
            &code_host,
            &args.config,
            &owner_identity,
            crate::adapters::failpoint::reach,
            output,
            true,
        );
        for language in languages(repo, args.language.as_deref())? {
            let started = std::time::Instant::now();
            let plan = orchestrator.plan_language(&language)?;
            let status = if plan.conflicts > 0 {
                Status::NeedsHuman
            } else if plan.deferred_units > 0 {
                Status::Partial
            } else {
                Status::Ok
            };
            if report::overall(&[outcome_for_status(worst), outcome_for_status(status)]) == status {
                worst = status;
            }
            tracing::info!(
                event = "locale.plan.completed",
                repository_id = %repository_trace_id(repo),
                locale = language,
                status = status.as_str(),
                source_revision_id = %crate::diagnostics::safe_id(&plan.source_revision),
                documents = plan.documents,
                pending_units = plan.pending_units,
                reused_units = plan.reused_units,
                conflicts = plan.conflicts,
                deferred_units = plan.deferred_units,
                duration_ms = started.elapsed().as_millis() as u64,
            );
            output.stdout(&format!(
                "{} [{}] source={} documents={} pending={} reused={} conflicts={} deferred={}",
                repo.path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("repo"),
                language,
                &plan.source_revision[..plan.source_revision.len().min(12)],
                plan.documents,
                plan.pending_units,
                plan.reused_units,
                plan.conflicts,
                plan.deferred_units,
            ));
        }
    }
    Ok(command_output(worst.exit_code()))
}

fn outcome_for_status(status: Status) -> LanguageOutcome {
    let mut outcome = LanguageOutcome::new(Path::new("."), "");
    outcome.status = status;
    outcome
}

fn sync(args: SyncRequest, output: &dyn OutputReporter) -> Result<CommandOutput> {
    let started = Local::now();
    let config = Config::load(&args.selection.config)?;
    config.check_environment()?;
    let repositories = selected(&config, args.selection.repository.as_deref())?;
    let report_dir = args
        .report_dir
        .unwrap_or_else(|| PathBuf::from(".fani-report"));
    let mut outcomes = Vec::new();
    let mut database_paths = Vec::new();
    tracing::info!(
        event = "sync.started",
        repository_count = repositories.len(),
        quiet = args.quiet,
    );

    for repo in repositories {
        let database = open_database(repo)?;
        database_paths.push(database.path().to_owned());
        let mut lock = match RepoLock::acquire(database.clone(), &repo.path) {
            Ok(lock) => lock,
            Err(error) => {
                for language in languages(repo, args.selection.language.as_deref())? {
                    let mut outcome = LanguageOutcome::new(&repo.path, &language);
                    outcome.status = Status::Error;
                    outcome.message = error.to_string();
                    outcome.transitions.push("error:lock_busy".into());
                    outcomes.push(outcome);
                }
                continue;
            }
        };
        let materializer = FilesystemMaterializer;
        let agents = RoutedAgentExecutor::new(&config);
        let documentation = NativeDocumentationChecker;
        let git = NativeGitPublisher;
        let code_host = GithubCodeHost;
        let owner_identity = owner_identity()?;
        let orchestrator = Orchestrator::new(
            repo,
            &database,
            &materializer,
            &agents,
            &documentation,
            &git,
            &code_host,
            &args.selection.config,
            &owner_identity,
            crate::adapters::failpoint::reach,
            output,
            args.quiet,
        );
        for language in languages(repo, args.selection.language.as_deref())? {
            let outcome = orchestrator.run_language(&language);
            tracing::info!(
                event = "locale.run.reported",
                repository_id = %repository_trace_id(repo),
                locale = %language,
                run_id = %crate::diagnostics::safe_id(&outcome.run_id),
                status = outcome.status.as_str(),
                duration_ms = (outcome.duration_s * 1000.0) as u64,
                agent_calls = outcome.agent_calls.len(),
                files_written = outcome.written.len(),
                findings = outcome.findings.len(),
            );
            if !args.quiet {
                output.stderr(&format!(
                    "  {} [{}] {}: {}",
                    repo.path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("repo"),
                    language,
                    outcome.status.as_str(),
                    outcome.message
                ));
            }
            outcomes.push(outcome);
        }
        lock.release()?;
    }

    let data = report::write(
        &outcomes,
        &report_dir,
        started,
        &args.selection.config,
        &database_paths,
    )?;
    if !args.quiet {
        output.stdout(data["status"].as_str().unwrap_or("error"));
        output.stderr(&format!(
            "report: {}",
            report_dir.join("report.md").display()
        ));
    }
    let overall = report::overall(&outcomes);
    tracing::info!(
        event = "sync.completed",
        status = overall.as_str(),
        locale_count = outcomes.len(),
    );
    Ok(command_output(overall.exit_code()))
}

fn doctor(config_path: PathBuf, output: &dyn OutputReporter) -> Result<CommandOutput> {
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(error) => {
            output.stdout(&format!("FAIL config: {error}"));
            return Ok(command_output(Status::Error.exit_code()));
        }
    };
    output.stdout(&format!("ok   config: {}", config_path.display()));
    let mut problems = 0;
    if let Err(error) = config.check_environment() {
        problems += 1;
        output.stdout(&format!("FAIL environment: {error}"));
    }
    if executable_on_path("git").is_none() {
        problems += 1;
        output.stdout("FAIL git: executable not found");
    } else {
        output.stdout("ok   git: executable found");
    }
    for (name, agent) in &config.agents {
        if agent.enabled && executable_on_path(&agent.cmd[0]).is_none() {
            problems += 1;
            output.stdout(&format!("FAIL Agent {name}: {} not found", agent.cmd[0]));
        } else {
            output.stdout(&format!("ok   Agent {name}: {}", agent.cmd[0]));
        }
    }
    for repo in &config.repos {
        for (index, command) in repo.documentation.commands.iter().enumerate() {
            if executable_on_path(&command[0]).is_none() {
                problems += 1;
                output.stdout(&format!(
                    "FAIL documentation check {}[{}]: {} not found",
                    repo.path.display(),
                    index,
                    command[0]
                ));
            } else {
                output.stdout(&format!(
                    "ok   documentation check {}[{}]: {}",
                    repo.path.display(),
                    index,
                    command[0]
                ));
            }
        }
    }
    for stage in ["translate", "repair", "revision", "proofread"] {
        let required = stage == "translate"
            || stage == "repair"
            || (stage == "revision" && config.repos.iter().any(|repo| repo.quality.revision))
            || (stage == "proofread" && config.repos.iter().any(|repo| repo.quality.proofread));
        match config.agent_for(stage) {
            Ok(agent) => output.stdout(&format!("ok   stage {stage} -> {}", agent.name)),
            Err(error) if required => {
                problems += 1;
                output.stdout(&format!("FAIL stage {stage}: {error}"));
            }
            Err(error) => output.stdout(&format!("note stage {stage}: {error}")),
        }
    }
    for repo in &config.repos {
        if repo.publish.github.enabled && executable_on_path("gh").is_none() {
            problems += 1;
            output.stdout(&format!(
                "FAIL GitHub {}: gh executable not found",
                repo.path.display()
            ));
        }
        match open_database(repo) {
            Ok(database) => {
                if let Err(error) = database.integrity_check() {
                    problems += 1;
                    output.stdout(&format!(
                        "FAIL database {}: {error}",
                        database.path().display()
                    ));
                } else {
                    output.stdout(&format!(
                        "ok   repository {}: SQLite {}, native schema {}",
                        repo.path.display(),
                        database.sqlite_version()?,
                        database.schema_version()?
                    ));
                }
            }
            Err(error) => {
                problems += 1;
                output.stdout(&format!(
                    "FAIL database {}: {error}",
                    database_path(repo).display()
                ));
            }
        }
    }
    let exit_code = if problems == 0 {
        output.stdout("\nready");
        Status::Ok.exit_code()
    } else {
        output.stdout(&format!(
            "\n{problems} problem(s) must be fixed before sync"
        ));
        Status::Error.exit_code()
    };
    Ok(command_output(exit_code))
}

fn reconcile(
    args: Selection,
    mode: ReconcileMode,
    output: &dyn OutputReporter,
) -> Result<CommandOutput> {
    let config = Config::load(&args.config)?;
    config.check_environment()?;
    let mut count = 0;
    for repo in selected(&config, args.repository.as_deref())? {
        let database = open_database(repo)?;
        let mut lock = RepoLock::acquire(database.clone(), &repo.path)?;
        let materializer = FilesystemMaterializer;
        let git = NativeGitPublisher;
        for language in languages(repo, args.language.as_deref())? {
            count += match mode {
                ReconcileMode::Adopt => {
                    adopt_human_edit(repo, &database, &materializer, &git, &language)?
                }
                ReconcileMode::Discard => {
                    discard_human_edit(repo, &database, &materializer, &git, &language)?
                }
            };
        }
        lock.release()?;
    }
    let action = match mode {
        ReconcileMode::Adopt => "adopted",
        ReconcileMode::Discard => "discarded",
    };
    output.stdout(&format!("{action} {count} target file(s)"));
    Ok(command_output(Status::Ok.exit_code()))
}

impl CommandOperations for NativeOperations<'_> {
    fn sync(&self, request: SyncRequest) -> Result<CommandOutput> {
        sync(request, self.output)
    }

    fn status(&self, request: Selection) -> Result<CommandOutput> {
        status(request, self.output)
    }

    fn doctor(&self, config: PathBuf) -> Result<CommandOutput> {
        doctor(config, self.output)
    }

    fn reconcile(&self, selection: Selection, mode: ReconcileMode) -> Result<CommandOutput> {
        reconcile(selection, mode, self.output)
    }
}
