use rusqlite::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const SOURCE: &str = "# Getting started\n\nThis is a short guide for new users.\n\n```bash\necho hello\n```\n\nRun the command above, then read the `config.toml` file.\n\n## Next steps\n\nNothing else to do.\n";

fn skill() -> PathBuf {
    if let Some(path) = std::env::var_os("FANI_TEST_SKILL") {
        return path.into();
    }
    PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".claude/skills/i18n")
}

struct Case {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    config: PathBuf,
    reports: PathBuf,
}

impl Case {
    fn new(mode: &str, timeout: f64, commit: bool) -> Self {
        let skill = skill();
        assert!(
            skill.join("scripts/run.sh").is_file(),
            "real i18n skill missing at {}",
            skill.display()
        );
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("docs/guide.md"), SOURCE).unwrap();
        let reports = tmp.path().join("reports");
        let config = tmp.path().join("fani.toml");
        let agent = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py");
        fs::write(
            &config,
            format!(
                r#"skill = "{}"
[[repo]]
path = "{}"
languages = ["zh-CN"]
state_dir = ".fani-state"
max_tasks = 5
repair_budget = 1
commit = {}
[agents.fake]
cmd = ["python3", "{}", "{}"]
stages = ["translate", "revision", "proofread"]
concurrency = 2
timeout_s = {}
retries = 0
[routing]
translate = "fake"
revision = "fake"
proofread = "fake"
"#,
                skill.display(),
                repo.display(),
                commit,
                agent.display(),
                mode,
                timeout
            ),
        )
        .unwrap();
        Self {
            _tmp: tmp,
            repo,
            config,
            reports,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo::cargo_bin!("fani"))
            .args(args)
            .output()
            .unwrap()
    }

    fn sync_command(&self, reports: &Path) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("fani"));
        command.args([
            "sync",
            "--config",
            self.config.to_str().unwrap(),
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ]);
        command
    }

    fn sync(&self) -> Output {
        self.sync_command(&self.reports).output().unwrap()
    }

    fn enable_proofread(&self, mode: &str) {
        let agent = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py");
        let config = fs::read_to_string(&self.config).unwrap();
        let config = config
            .replace(
                "commit = false\n[agents.fake]",
                "commit = false\n[repo.stages]\nproofread = true\n[agents.fake]",
            )
            .replace("proofread = \"fake\"", "proofread = \"reviewer\"");
        let config = format!(
            "{config}\n[agents.reviewer]\ncmd = [\"python3\", \"{}\", \"{mode}\"]\nstages = [\"proofread\"]\nconcurrency = 1\ntimeout_s = 30\nretries = 0\n",
            agent.display()
        );
        fs::write(&self.config, config).unwrap();
    }

    fn report(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.reports.join("report.json")).unwrap())
            .unwrap()
    }

    fn db(&self) -> Connection {
        Connection::open(self.repo.join(".fani-state/fani.db")).unwrap()
    }
}

#[test]
fn real_cli_doctor_status_sync_restart_and_human_edit() {
    let case = Case::new("ok", 30.0, false);
    let doctor = case.run(&["doctor", "--config", case.config.to_str().unwrap()]);
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&doctor.stdout)
    );
    let doctor_text = String::from_utf8_lossy(&doctor.stdout);
    assert!(doctor_text.contains("SQLite ") && doctor_text.contains("schema 2"));

    let status = case.run(&["status", "--config", case.config.to_str().unwrap()]);
    assert_eq!(status.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&status.stdout).contains("tasks="));
    assert!(!case.repo.join("docs/guide.zh-CN.md").exists());
    fs::create_dir_all(case.repo.join(".fani-state/work/legacy-run")).unwrap();
    fs::write(
        case.repo
            .join(".fani-state/work/legacy-run/dispatch.jsonl"),
        "{\"task_id\":\"legacy-a\",\"ok\":true}\n{\"task_id\":\n{\"task_id\":\"legacy-b\",\"ok\":false}\n",
    )
    .unwrap();

    let first = case.sync();
    assert_eq!(
        first.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    let target = case.repo.join("docs/guide.zh-CN.md");
    let translated = fs::read_to_string(&target).unwrap();
    assert!(translated.contains("[zh]"));
    assert!(translated.contains("echo hello"));
    assert!(translated.contains("`config.toml`"));
    assert_eq!(case.report()["totals"]["agent_calls"], 1);
    assert!(case.repo.join(".fani-state/fani.db").is_file());
    let legacy_rows: (i64, i64) = case
        .db()
        .query_row(
            "SELECT COUNT(*), SUM(kind LIKE '%-invalid') FROM legacy_imports",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(legacy_rows, (3, 1));
    let persisted: String = case
        .db()
        .query_row(
            "SELECT transitions_json FROM language_runs ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        persisted.contains("dispatching")
            && persisted.contains("verifying")
            && persisted.contains("complete:ok")
    );

    let second = case.sync();
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(case.report()["totals"]["agent_calls"], 0);
    assert_eq!(
        case.report()["languages"][0]["message"],
        "every translation is up to date"
    );
    let runs: i64 = case
        .db()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert!(runs >= 2);

    let edited = translated + "\n人工补充的一段。\n";
    fs::write(&target, &edited).unwrap();
    let conflict = case.sync();
    assert_eq!(conflict.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&target).unwrap(), edited);
    assert_eq!(case.report()["status"], "needs_human");
}

#[test]
fn agent_exit_timeout_and_structural_failure_never_publish_bad_translation() {
    for (mode, timeout, expected_code) in [("fail", 30.0, "DSP-EXIT"), ("slow", 0.2, "DSP-TIMEOUT")]
    {
        let case = Case::new(mode, timeout, false);
        let out = case.sync();
        assert_eq!(
            out.status.code(),
            Some(1),
            "mode={mode} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!case.repo.join("docs/guide.zh-CN.md").exists());
        assert_eq!(
            case.report()["languages"][0]["dispatch"][0]["code"],
            expected_code
        );
        let markdown = fs::read_to_string(case.reports.join("report.md")).unwrap();
        assert!(markdown.contains("Failed agent calls:"));
        assert!(markdown.contains(expected_code));
        let calls: i64 = case
            .db()
            .query_row("SELECT COUNT(*) FROM agent_calls WHERE ok=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(calls >= 1);
    }

    let case = Case::new("mangle", 30.0, false);
    let out = case.sync();
    assert_eq!(out.status.code(), Some(1));
    assert!(!case.repo.join("docs/guide.zh-CN.md").exists());
    assert!(
        case.report()["languages"][0]["message"]
            .as_str()
            .unwrap()
            .contains("assembled")
    );
}

#[test]
fn real_skill_verify_failure_enters_one_repair_round() {
    let case = Case::new("verifyrepair", 30.0, false);
    let counter = case.repo.join("agent-counter");
    let config = fs::read_to_string(&case.config).unwrap();
    let config = config.replace(
        "cmd = [\"python3\",",
        &format!(
            "cmd = [\"env\", \"FAKE_AGENT_COUNTER={}\", \"python3\",",
            counter.display()
        ),
    );
    fs::write(&case.config, config).unwrap();

    let out = case.sync();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = case.report();
    let language = &report["languages"][0];
    assert_eq!(language["repair_rounds"], 1);
    assert_eq!(language["dispatch"].as_array().unwrap().len(), 2);
    let transitions: String = case
        .db()
        .query_row(
            "SELECT transitions_json FROM language_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(transitions.contains("repairing:verify:1"));
    assert_eq!(fs::read_to_string(counter).unwrap(), "2");
    assert!(case.repo.join("docs/guide.zh-CN.md").is_file());
}

#[test]
fn genuine_proofread_tasks_reach_the_agent() {
    let case = Case::new("ok", 30.0, false);
    case.enable_proofread("review");
    let out = case.sync();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(case.report()["totals"]["agent_calls"], 2);
    assert_eq!(
        case.report()["languages"][0]["dispatch"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn advisory_proofread_failure_is_visible_in_markdown_report() {
    let case = Case::new("ok", 30.0, false);
    case.enable_proofread("fail");
    let out = case.sync();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(case.report()["status"], "ok");
    let markdown = fs::read_to_string(case.reports.join("report.md")).unwrap();
    assert!(markdown.contains("Failed agent calls:"));
    assert!(markdown.contains("DSP-EXIT"));
}

#[test]
fn overlapping_cli_sync_is_rejected_by_repository_lock() {
    let case = Case::new("pause", 30.0, false);
    let first_reports = case.reports.join("first");
    let second_reports = case.reports.join("second");
    let mut first = case.sync_command(&first_reports).spawn().unwrap();
    let lock_path = case.repo.join(".fani-state/fani.lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !lock_path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        lock_path.is_file(),
        "first sync never acquired the repository lock"
    );

    let second = case.sync_command(&second_reports).output().unwrap();
    assert_eq!(second.status.code(), Some(2));
    assert!(!case.repo.join("docs/guide.zh-CN.md").exists());
    let second_report: Value =
        serde_json::from_str(&fs::read_to_string(second_reports.join("report.json")).unwrap())
            .unwrap();
    assert!(
        second_report["languages"][0]["message"]
            .as_str()
            .unwrap()
            .contains("another run")
    );

    let first_status = first.wait().unwrap();
    assert_eq!(first_status.code(), Some(0));
    assert!(case.repo.join("docs/guide.zh-CN.md").is_file());
}

#[test]
fn publish_failure_requires_human_without_changing_error_contract() {
    let case = Case::new("ok", 30.0, true);
    let out = case.sync();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(case.report()["status"], "needs_human");
    assert!(
        case.report()["languages"][0]["message"]
            .as_str()
            .unwrap()
            .contains("could not commit")
    );
    let transitions: String = case
        .db()
        .query_row(
            "SELECT transitions_json FROM language_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(transitions.contains("needs_human:publish"));
    assert!(case.repo.join("docs/guide.zh-CN.md").is_file());
}

#[test]
fn doctor_rejects_non_executable_uv_and_agent_files() {
    let case = Case::new("ok", 30.0, false);
    let bin_dir = case.repo.parent().unwrap().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let uv = bin_dir.join("uv");
    let agent = bin_dir.join("fake-agent");
    fs::write(&uv, "not executable\n").unwrap();
    fs::write(&agent, "not executable\n").unwrap();
    for path in [&uv, &agent] {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(path, permissions).unwrap();
    }
    let config = fs::read_to_string(&case.config).unwrap();
    let config = config
        .lines()
        .map(|line| {
            if line.starts_with("cmd = ") {
                format!("cmd = [\"{}\"]", agent.display())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&case.config, config).unwrap();

    let out = Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .args(["doctor", "--config", case.config.to_str().unwrap()])
        .env("PATH", &bin_dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("FAIL uv: not on PATH"));
    assert!(stdout.contains("FAIL agent fake"));
    assert!(!stdout.contains("\nready"));
}

#[test]
fn real_sync_git_publish_does_not_move_checkout_or_commit_database() {
    let case = Case::new("ok", 30.0, true);
    let git = |args: &[&str]| -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&case.repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@e"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "sources"]);
    let before = git(&["rev-parse", "HEAD"]);
    fs::write(case.repo.join("staged.txt"), "keep staged\n").unwrap();
    git(&["add", "staged.txt"]);
    let index_tree_before = git(&["write-tree"]);
    let staged_before = git(&["diff", "--cached", "--raw"]);
    let resolved_index = PathBuf::from(git(&["rev-parse", "--git-path", "index"]));
    let index_path = if resolved_index.is_absolute() {
        resolved_index
    } else {
        case.repo.join(resolved_index)
    };
    let index_bytes_before = fs::read(&index_path).unwrap();
    let index_sha_before = format!("{:x}", Sha256::digest(&index_bytes_before));
    fs::write(case.repo.join("unrelated.txt"), "mine\n").unwrap();
    let out = case.sync();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let index_bytes_after = fs::read(&index_path).unwrap();
    let index_sha_after = format!("{:x}", Sha256::digest(&index_bytes_after));
    assert_eq!(index_bytes_after, index_bytes_before);
    let head_after = git(&["rev-parse", "HEAD"]);
    let branch_after = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let index_tree_after = git(&["write-tree"]);
    let staged_after = git(&["diff", "--cached", "--raw"]);
    assert_eq!(head_after, before);
    assert_eq!(branch_after, "main");
    assert_eq!(index_tree_after, index_tree_before);
    assert_eq!(staged_after, staged_before);
    println!(
        "GIT_ISOLATION head_before={before} head_after={head_after} branch_after={branch_after} index_sha_before={index_sha_before} index_sha_after={index_sha_after} index_tree_before={index_tree_before} index_tree_after={index_tree_after}"
    );
    let files = git(&["ls-tree", "-r", "--name-only", "i18n/zh-CN"]);
    assert!(files.contains("docs/guide.zh-CN.md"));
    assert!(files.contains(".fani-state/state.json"));
    assert!(!files.contains("fani.db"));
    assert!(!files.contains("staged.txt"));
    assert!(!files.contains("unrelated.txt"));
}
