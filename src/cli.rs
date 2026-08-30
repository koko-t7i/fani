use crate::VERSION;
use crate::agent::executable_on_path;
use crate::config::{Config, RepoConfig};
use crate::db::Database;
use crate::lock::RepoLock;
use crate::model::{LanguageOutcome, Status};
use crate::orchestrator::{Orchestrator, adopt_human_edit, discard_human_edit};
use crate::report;
use anyhow::{Result, anyhow};
use chrono::Local;
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(name = "fani", version = VERSION, about = "Native continuous Markdown translation for Git and GitHub")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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

#[derive(Clone, Debug, Args)]
struct CommonArgs {
    #[arg(long, default_value = "fani.toml")]
    config: PathBuf,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    lang: Option<String>,
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

pub fn run() -> i32 {
    match run_inner(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fani: {error:#}");
            Status::Error.exit_code()
        }
    }
}

fn run_inner(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Sync(args) => sync(args),
        Command::Status(args) | Command::Check(args) => status(args),
        Command::Doctor(args) => doctor(args),
        Command::Adopt(args) => reconcile(args, true),
        Command::Discard(args) => reconcile(args, false),
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

fn open_database(repo: &RepoConfig) -> Result<Database> {
    Database::open(database_path(repo))
}

fn status(args: CommonArgs) -> Result<i32> {
    let config = Config::load(&args.config)?;
    config.check_environment()?;
    let mut worst = Status::Ok;
    for repo in selected(&config, args.repo.as_deref())? {
        let snapshot_dir = tempfile::tempdir()?;
        let snapshot_path = snapshot_dir.path().join("fani.db");
        let authoritative_path = database_path(repo);
        if authoritative_path.exists() {
            Database::snapshot(&authoritative_path, &snapshot_path)?;
        }
        let database = Database::open(&snapshot_path)?;
        let orchestrator = Orchestrator::new(&config, repo, &database, &args.config, true);
        for language in languages(repo, args.lang.as_deref())? {
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
            println!(
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
            );
        }
    }
    Ok(worst.exit_code())
}

fn outcome_for_status(status: Status) -> LanguageOutcome {
    let mut outcome = LanguageOutcome::new(Path::new("."), "");
    outcome.status = status;
    outcome
}

fn sync(args: SyncArgs) -> Result<i32> {
    let started = Local::now();
    let config = Config::load(&args.common.config)?;
    config.check_environment()?;
    let repositories = selected(&config, args.common.repo.as_deref())?;
    let report_dir = args
        .report_dir
        .unwrap_or_else(|| PathBuf::from(".fani-report"));
    let mut outcomes = Vec::new();
    let mut database_paths = Vec::new();

    for repo in repositories {
        let database = open_database(repo)?;
        database_paths.push(database.path().to_owned());
        let mut lock = match RepoLock::acquire(database.clone(), &repo.path) {
            Ok(lock) => lock,
            Err(error) => {
                for language in languages(repo, args.common.lang.as_deref())? {
                    let mut outcome = LanguageOutcome::new(&repo.path, &language);
                    outcome.status = Status::Error;
                    outcome.message = error.to_string();
                    outcome.transitions.push("error:lock_busy".into());
                    outcomes.push(outcome);
                }
                continue;
            }
        };
        let orchestrator =
            Orchestrator::new(&config, repo, &database, &args.common.config, args.quiet);
        for language in languages(repo, args.common.lang.as_deref())? {
            let outcome = orchestrator.run_language(&language);
            if !args.quiet {
                eprintln!(
                    "  {} [{}] {}: {}",
                    repo.path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("repo"),
                    language,
                    outcome.status.as_str(),
                    outcome.message
                );
            }
            outcomes.push(outcome);
        }
        lock.release()?;
    }

    let data = report::write(
        &outcomes,
        &report_dir,
        started,
        &args.common.config,
        &database_paths,
    )?;
    if !args.quiet {
        println!("{}", data["status"].as_str().unwrap_or("error"));
        eprintln!("report: {}", report_dir.join("report.md").display());
    }
    Ok(report::overall(&outcomes).exit_code())
}

fn doctor(args: DoctorArgs) -> Result<i32> {
    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(error) => {
            println!("FAIL config: {error}");
            return Ok(Status::Error.exit_code());
        }
    };
    println!("ok   config: {}", args.config.display());
    let mut problems = 0;
    if let Err(error) = config.check_environment() {
        problems += 1;
        println!("FAIL environment: {error}");
    }
    if executable_on_path("git").is_none() {
        problems += 1;
        println!("FAIL git: executable not found");
    } else {
        println!("ok   git: executable found");
    }
    for (name, agent) in &config.agents {
        if agent.enabled && executable_on_path(&agent.cmd[0]).is_none() {
            problems += 1;
            println!("FAIL Agent {name}: {} not found", agent.cmd[0]);
        } else {
            println!("ok   Agent {name}: {}", agent.cmd[0]);
        }
    }
    for stage in ["translate", "repair", "revision", "proofread"] {
        let required = stage == "translate"
            || stage == "repair"
            || (stage == "revision" && config.repos.iter().any(|repo| repo.quality.revision))
            || (stage == "proofread" && config.repos.iter().any(|repo| repo.quality.proofread));
        match config.agent_for(stage) {
            Ok(agent) => println!("ok   stage {stage} -> {}", agent.name),
            Err(error) if required => {
                problems += 1;
                println!("FAIL stage {stage}: {error}");
            }
            Err(error) => println!("note stage {stage}: {error}"),
        }
    }
    for repo in &config.repos {
        if repo.publish.github.enabled && executable_on_path("gh").is_none() {
            problems += 1;
            println!(
                "FAIL GitHub {}: gh executable not found",
                repo.path.display()
            );
        }
        match open_database(repo) {
            Ok(database) => {
                if let Err(error) = database.integrity_check() {
                    problems += 1;
                    println!("FAIL database {}: {error}", database.path().display());
                } else {
                    println!(
                        "ok   repository {}: SQLite {}, native schema {}",
                        repo.path.display(),
                        database.sqlite_version()?,
                        database.schema_version()?
                    );
                }
            }
            Err(error) => {
                problems += 1;
                println!("FAIL database {}: {error}", database_path(repo).display());
            }
        }
    }
    if problems == 0 {
        println!("\nready");
        Ok(Status::Ok.exit_code())
    } else {
        println!("\n{problems} problem(s) must be fixed before sync");
        Ok(Status::Error.exit_code())
    }
}

fn reconcile(args: CommonArgs, adopt: bool) -> Result<i32> {
    let config = Config::load(&args.config)?;
    config.check_environment()?;
    let mut count = 0;
    for repo in selected(&config, args.repo.as_deref())? {
        let database = open_database(repo)?;
        let mut lock = RepoLock::acquire(database.clone(), &repo.path)?;
        for language in languages(repo, args.lang.as_deref())? {
            count += if adopt {
                adopt_human_edit(repo, &database, &language)?
            } else {
                discard_human_edit(repo, &database, &language)?
            };
        }
        lock.release()?;
    }
    println!(
        "{} {count} target file(s)",
        if adopt { "adopted" } else { "discarded" }
    );
    Ok(Status::Ok.exit_code())
}
