use crate::VERSION;
use crate::config::{Config, RepoConfig};
use crate::db::Database;
use crate::lock::RepoLock;
use crate::model::{LangOutcome, Status};
use crate::orchestrator::Orchestrator;
use crate::report;
use crate::skill::Skill;
use anyhow::{Result, anyhow};
use chrono::Local;
use clap::{Args, Parser, Subcommand};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(name = "fani", version = VERSION, about = "Scheduled documentation translation using the i18n skill.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Translate everything that is out of date.
    Sync(SyncArgs),
    /// Show what a sync would do; calls no agent.
    Status(CommonArgs),
    /// Check config, skill, uv and agent binaries.
    Doctor(DoctorArgs),
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
        Err(err) => {
            eprintln!("fani: {err}");
            2
        }
    }
}

fn run_inner(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Sync(args) => sync(args),
        Command::Status(args) => status(args),
        Command::Doctor(args) => doctor(args),
    }
}

fn selected<'a>(cfg: &'a Config, filter: Option<&str>) -> Result<Vec<&'a RepoConfig>> {
    let Some(filter) = filter else {
        return Ok(cfg.repos.iter().collect());
    };
    let selected: Vec<_> = cfg
        .repos
        .iter()
        .filter(|repo| {
            repo.path.file_name().and_then(|x| x.to_str()) == Some(filter)
                || repo.path.display().to_string() == filter
        })
        .collect();
    if selected.is_empty() {
        return Err(anyhow!(
            "no repo matches {filter:?} (configured: {})",
            cfg.repos
                .iter()
                .filter_map(|r| r.path.file_name()?.to_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(selected)
}

fn languages(repo: &RepoConfig, filter: Option<&str>) -> Result<Vec<String>> {
    let Some(filter) = filter else {
        return Ok(repo.languages.clone());
    };
    if repo.languages.iter().any(|x| x == filter) {
        return Ok(vec![filter.into()]);
    }
    Err(anyhow!(
        "{} is not configured for {filter:?} (configured: {})",
        repo.path
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("repo"),
        repo.languages.join(", ")
    ))
}

fn database(repo: &RepoConfig) -> Result<Database> {
    Database::open(repo.path.join(&repo.state_dir).join("fani.db"))
}

fn import_legacy(db: &Database, repo: &RepoConfig) -> Result<()> {
    db.import_legacy_json(
        &repo.path.join(&repo.state_dir).join("state.json"),
        "external-skill-state",
    )?;
    let work = repo.path.join(&repo.state_dir).join("work");
    if let Ok(runs) = fs::read_dir(work) {
        for run in runs.flatten() {
            db.import_legacy_jsonl(&run.path().join("dispatch.jsonl"), "python-dispatch-record")?;
            db.import_legacy_json(&run.path().join("verify.json"), "python-verify-result")?;
        }
    }
    Ok(())
}

fn sync(args: SyncArgs) -> Result<i32> {
    let started = Local::now();
    let cfg = Config::load(&args.common.config)?;
    cfg.check_environment()?;
    let repos = selected(&cfg, args.common.repo.as_deref())?;
    let report_dir = args
        .report_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(".fani"));
    let report_dir = if report_dir.is_absolute() {
        report_dir
    } else {
        std::env::current_dir()?.join(report_dir)
    };
    let mut outcomes = Vec::new();
    let mut db_paths = Vec::new();

    for repo in repos {
        let langs = languages(repo, args.common.lang.as_deref())?;
        let db = database(repo)?;
        import_legacy(&db, repo)?;
        db_paths.push(db.path().to_path_buf());
        let db_run = db.start_run(&args.common.config)?;
        let lock = RepoLock::acquire(db.clone(), &repo.path);
        let mut lock = match lock {
            Ok(lock) => lock,
            Err(err) => {
                let mut out = LangOutcome::new(&repo.path, &langs.join(","));
                out.status = Status::Error;
                out.message = err.to_string();
                out.transitions.push("error:lock_busy".into());
                db.record_language(&db_run, &out)?;
                db.finish_run(&db_run, out.status.as_str(), out.status.exit_code())?;
                outcomes.push(out);
                continue;
            }
        };
        db.recover_incomplete_runs(&db_run)?;
        let before = outcomes.len();
        let orch = Orchestrator::new(&cfg, repo, &db, &db_run, args.quiet);
        for lang in langs {
            let out = orch.run_language(&lang);
            if !args.quiet {
                eprintln!(
                    "  {} [{lang}] {}: {}",
                    repo.path
                        .file_name()
                        .and_then(|x| x.to_str())
                        .unwrap_or("repo"),
                    out.status.as_str(),
                    out.message
                );
            }
            db.record_language(&db_run, &out)?;
            outcomes.push(out);
        }
        lock.release()?;
        let repo_status = report::overall(&outcomes[before..]);
        db.finish_run(&db_run, repo_status.as_str(), repo_status.exit_code())?;
    }

    for repo in &cfg.repos {
        if report_dir.starts_with(&repo.path) && !args.quiet {
            eprintln!(
                "warning: reports are written inside {} ({}); move --report-dir outside the repository or exclude it",
                repo.path.display(),
                report_dir.display()
            );
        }
    }
    let data = report::write(
        &outcomes,
        &report_dir,
        started,
        &args.common.config,
        &db_paths,
    )?;
    if !args.quiet {
        eprintln!("report: {}", report_dir.join("report.md").display());
        println!("{}", data["status"].as_str().unwrap_or("error"));
    }
    Ok(report::overall(&outcomes).exit_code())
}

fn status(args: CommonArgs) -> Result<i32> {
    let cfg = Config::load(&args.config)?;
    cfg.check_environment()?;
    let mut worst = Status::Ok;
    for repo in selected(&cfg, args.repo.as_deref())? {
        let skill = Skill::new(&cfg.skill, &repo.path, &repo.state_dir);
        for lang in languages(repo, args.lang.as_deref())? {
            let plan = skill
                .plan(&lang, &repo.paths, &repo.exclude, repo.max_tasks, None)?
                .data;
            let conflicts = plan.conflicts.len();
            if conflicts > 0 {
                worst = Status::NeedsHuman;
            }
            println!(
                "{} [{lang}] tasks={} files={} conflicts={} reused={} deferred={}",
                repo.path
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or("repo"),
                plan.task_count,
                plan.files.len(),
                conflicts,
                plan.fuzzy_matched,
                plan.truncated_tasks
            );
        }
    }
    Ok(worst.exit_code())
}

fn doctor(args: DoctorArgs) -> Result<i32> {
    let cfg = match Config::load(&args.config) {
        Ok(cfg) => cfg,
        Err(err) => {
            println!("FAIL config: {err}");
            return Ok(2);
        }
    };
    println!("ok   config: {}", args.config.display());
    let mut problems = 0;
    if let Err(err) = cfg.check_environment() {
        problems += 1;
        println!("FAIL environment: {err}");
    } else {
        println!("ok   skill: {}", cfg.skill.join("scripts/run.sh").display());
    }
    if find_binary("uv").is_none() {
        problems += 1;
        println!("FAIL uv: not on PATH");
    } else {
        println!("ok   uv: on PATH");
    }
    for (name, agent) in &cfg.agents {
        let found = find_binary(&agent.cmd[0]);
        if found.is_none() && agent.enabled {
            problems += 1;
            println!("FAIL agent {name}: {} not on PATH", agent.cmd[0]);
        } else {
            println!(
                "ok   agent {name} ({}): {}",
                if agent.enabled { "enabled" } else { "disabled" },
                found
                    .unwrap_or_else(|| PathBuf::from(&agent.cmd[0]))
                    .display()
            );
        }
    }
    for stage in ["translate", "revision", "proofread"] {
        let wanted = cfg.repos.iter().any(|r| {
            stage == "translate"
                || (stage == "revision" && r.stages.revision)
                || (stage == "proofread" && r.stages.proofread)
        });
        match cfg.agent_for(stage) {
            Ok(agent) => println!("ok   stage {stage} -> {}", agent.name),
            Err(err) if wanted => {
                problems += 1;
                println!("FAIL stage {stage}: {err}");
            }
            Err(err) => println!("note stage {stage}: {err}"),
        }
    }
    for repo in &cfg.repos {
        match database(repo) {
            Ok(db) => {
                let sqlite = db.sqlite_version()?;
                if !sqlite_at_least(&sqlite, (3, 51, 3)) {
                    problems += 1;
                    println!(
                        "FAIL repo {}: SQLite {sqlite} is older than required 3.51.3",
                        repo.path.display()
                    );
                } else {
                    println!(
                        "ok   repo {}: {} (SQLite {sqlite}, schema {})",
                        repo.path
                            .file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or("repo"),
                        repo.languages.join(", "),
                        db.schema_version()?
                    );
                }
            }
            Err(err) => {
                problems += 1;
                println!("FAIL repo {}: {err}", repo.path.display());
            }
        }
    }
    if problems > 0 {
        println!("\n{problems} problem(s) must be fixed before a run.");
        Ok(2)
    } else {
        println!("\nready");
        Ok(0)
    }
}

fn sqlite_at_least(version: &str, minimum: (u64, u64, u64)) -> bool {
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0));
    let actual = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    actual >= minimum
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn find_binary(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return is_executable_file(path).then(|| path.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|path| path.join(name))
        .find(|path| is_executable_file(path))
}
