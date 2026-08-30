use rusqlite::{Connection, params};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::{TempDir, tempdir};

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn workflow_jobs(source: &str) -> BTreeMap<String, String> {
    let mut jobs = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    let jobs_source = source.split_once("\njobs:\n").unwrap().1;

    for line in jobs_source.lines() {
        if line.starts_with("  ") && !line.starts_with("    ") && line.ends_with(':') {
            if let Some((name, block)) = current.take() {
                jobs.insert(name, block);
            }
            current = Some((line.trim_end_matches(':').trim().to_string(), String::new()));
        } else if let Some((_, block)) = current.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    if let Some((name, block)) = current {
        jobs.insert(name, block);
    }
    jobs
}

fn checked(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "command failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        command,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    checked(Command::new("git").current_dir(cwd).args(args))
}

fn git_input(cwd: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git failed: {args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_config(clone: &Path) -> PathBuf {
    let config = clone.join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["README.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
repair_budget = 0
[repo.quality]
revision = false
proofread = false
[repo.publish]
enabled = false
source_ref = "HEAD"
[agents.fixture]
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
cmd = ["true"]
timeout_s = 5
retries = 0
[routing]
translate = "fixture"
repair = "fixture"
"#,
            clone.display()
        ),
    )
    .unwrap();
    config
}

fn state(clone: &Path, operation: &str, success: bool) -> Output {
    let config = clone.join("fani.toml");
    let output = Command::new(root().join("scripts/ci/state-ref.sh"))
        .current_dir(clone)
        .arg(operation)
        .env("FANI_BIN", env!("CARGO_BIN_EXE_fani"))
        .env("FANI_CONFIG_PATH", config)
        .env("FANI_STATE_REMOTE", "origin")
        .env("FANI_STATE_REF", "refs/heads/fani-state")
        .env("FANI_STATE_DB", ".fani/fani.db")
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "state {operation} unexpected status\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn initialize_database(clone: &Path) {
    let output = checked(
        Command::new(env!("CARGO_BIN_EXE_fani"))
            .arg("doctor")
            .arg("--config")
            .arg(clone.join("fani.toml")),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("SQLite"));
}

fn write_marker(database: &Path, value: &str) {
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS ci_contract_state(value TEXT NOT NULL);\
             DELETE FROM ci_contract_state;",
        )
        .unwrap();
    connection
        .execute("INSERT INTO ci_contract_state VALUES (?1)", params![value])
        .unwrap();
}

fn read_marker(database: &Path) -> String {
    Connection::open(database)
        .unwrap()
        .query_row("SELECT value FROM ci_contract_state", [], |row| row.get(0))
        .unwrap()
}

struct GitFixture {
    _temp: TempDir,
    root: PathBuf,
    remote: PathBuf,
    a: PathBuf,
    b: PathBuf,
}

impl GitFixture {
    fn new() -> Self {
        let temp = tempdir().unwrap();
        let fixture_root = temp.path().to_path_buf();
        let remote = fixture_root.join("remote.git");
        let source = fixture_root.join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "CI Contract"]);
        git(
            &source,
            &["config", "user.email", "ci-contract@example.invalid"],
        );
        fs::write(source.join("README.md"), "# Source\n").unwrap();
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "source"]);
        git(&fixture_root, &["init", "--bare", remote.to_str().unwrap()]);
        git(
            &source,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&source, &["push", "origin", "main"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let a = fixture_root.join("a");
        let b = fixture_root.join("b");
        git(
            &fixture_root,
            &["clone", remote.to_str().unwrap(), a.to_str().unwrap()],
        );
        git(
            &fixture_root,
            &["clone", remote.to_str().unwrap(), b.to_str().unwrap()],
        );
        for clone in [&a, &b] {
            git(clone, &["config", "user.name", "CI Contract"]);
            git(
                clone,
                &["config", "user.email", "ci-contract@example.invalid"],
            );
            write_config(clone);
        }

        Self {
            _temp: temp,
            root: fixture_root,
            remote,
            a,
            b,
        }
    }

    fn clone(&self, name: &str) -> PathBuf {
        let clone = self.root.join(name);
        git(
            &self.root,
            &[
                "clone",
                self.remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(&clone, &["config", "user.name", "CI Contract"]);
        git(
            &clone,
            &["config", "user.email", "ci-contract@example.invalid"],
        );
        write_config(&clone);
        clone
    }
}

#[derive(Deserialize)]
struct EventFixture {
    name: String,
    event: String,
    r#ref: String,
    default_branch: String,
    fork: Option<bool>,
    allowed: bool,
}

#[test]
fn recorded_events_gate_provider_execution_and_reject_fork_prs() {
    let fixtures: Vec<EventFixture> = serde_json::from_str(
        &fs::read_to_string(root().join("tests/fixtures/ci-events.json")).unwrap(),
    )
    .unwrap();
    let gate = root().join("scripts/ci/trusted-provider-gate.sh");
    let temp = tempdir().unwrap();

    for fixture in fixtures {
        let marker = temp.path().join(fixture.name.replace(' ', "-"));
        let gated = Command::new(&gate)
            .env("GITHUB_EVENT_NAME", &fixture.event)
            .env("GITHUB_REF", &fixture.r#ref)
            .env("GITHUB_DEFAULT_BRANCH", &fixture.default_branch)
            .output()
            .unwrap();
        if gated.status.success() {
            checked(
                Command::new("sh")
                    .arg("-c")
                    .arg(": > \"$MODEL_MARKER\"")
                    .env("MODEL_MARKER", &marker),
            );
        }
        assert_eq!(gated.status.success(), fixture.allowed, "{}", fixture.name);
        assert_eq!(marker.exists(), fixture.allowed, "{}", fixture.name);
        if fixture.fork == Some(true) {
            assert!(!gated.status.success(), "fork PR reached provider gate");
            assert!(!marker.exists(), "fork PR executed provider fixture");
        }
    }
}

#[test]
fn workflows_and_scripts_enforce_static_security_contracts() {
    let workflows = root().join(".github/workflows");
    let provider = fs::read_to_string(workflows.join("provider-sync.yml")).unwrap();
    let trigger_block = provider.split("permissions:").next().unwrap();
    assert!(!trigger_block.contains("pull_request"));
    for required in [
        "branches: [main]",
        "schedule:",
        "workflow_dispatch:",
        "permissions: {}",
        "contents: write",
        "github.event.repository.default_branch",
        "github.event_name == 'push'",
        "github.event_name == 'schedule'",
        "github.event_name == 'workflow_dispatch'",
        "persist-credentials: false",
        "group: provider-state-${{ github.repository }}",
        "cancel-in-progress: false",
        "refs/heads/fani-state",
        "state-ref.sh restore",
        "state-ref.sh publish",
        "FANI_BIN: target/release/fani",
    ] {
        assert!(provider.contains(required), "missing {required}");
    }
    for forbidden in ["pull-requests: write", "id-token: write", "actions/cache"] {
        assert!(!provider.contains(forbidden), "found {forbidden}");
    }
    assert_eq!(provider.matches("secrets.ANTHROPIC_API_KEY").count(), 1);
    let provider_step = provider
        .split("- name: Run provider-backed sync")
        .nth(1)
        .unwrap()
        .split("- name: Validate and publish")
        .next()
        .unwrap();
    assert!(!provider_step.contains("github.token"));
    assert!(!provider_step.contains("FANI_STATE_TOKEN"));

    let ordinary_ci = fs::read_to_string(workflows.join("ci.yml")).unwrap();
    assert!(ordinary_ci.contains("pull_request:"));
    assert_eq!(ordinary_ci.matches("permissions:").count(), 1);
    assert!(ordinary_ci.contains("permissions:\n  contents: read"));
    assert!(ordinary_ci.contains("cargo test --locked --test reproducible_candidate"));
    let jobs = workflow_jobs(&ordinary_ci);
    assert_eq!(
        jobs.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "git-contract",
            "linux-process-contract",
            "msrv",
            "native-cli-e2e",
            "quality",
            "reproducible-release",
            "sqlite-recovery",
        ]
    );
    for (job, check_name, command) in [
        (
            "native-cli-e2e",
            "Native CLI E2E",
            "cargo test --locked --test native_cli",
        ),
        (
            "sqlite-recovery",
            "SQLite Recovery",
            "cargo test --locked --test sqlite_recovery",
        ),
        (
            "git-contract",
            "Git Contract",
            "cargo test --locked --test git_publication",
        ),
        (
            "linux-process-contract",
            "Linux Process Contract",
            "cargo test --locked --test process_wrapper",
        ),
    ] {
        let block = &jobs[job];
        assert!(
            block.contains(&format!("    name: {check_name}\n")),
            "{job}"
        );
        assert!(block.contains("    runs-on: ubuntu-latest\n"), "{job}");
        assert!(block.contains("    timeout-minutes: 20\n"), "{job}");
        assert!(
            block.contains(
                "      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683\n"
            ),
            "{job}"
        );
        assert!(
            block.contains("          persist-credentials: false\n"),
            "{job}"
        );
        assert!(
            block.contains(&format!("        run: {command}\n")),
            "{job}"
        );
        assert_eq!(block.matches("cargo test ").count(), 1, "{job}");
    }
    assert!(jobs["quality"].contains("    name: Quality\n"));
    assert!(jobs["quality"].contains("cargo clippy --locked --all-targets --all-features"));
    assert!(jobs["quality"].contains("cargo fmt --all -- --check"));
    assert!(jobs["msrv"].contains("    name: MSRV\n"));
    assert!(jobs["msrv"].contains("cargo check --locked --all-targets"));
    let reproducible = &jobs["reproducible-release"];
    assert!(reproducible.contains("    name: Reproducible Release\n"));
    assert!(reproducible.contains("    runs-on: ubuntu-24.04\n"));
    assert!(reproducible.contains("sudo install -d -m 0755 /workspace"));
    assert!(reproducible.contains("cargo install --locked cargo-cyclonedx@0.5.9"));
    assert!(reproducible.contains("scripts/release/install-reproducible-dist.sh"));
    assert!(reproducible.contains("scripts/release/verify-reproducible-release.sh"));
    for forbidden in [
        "secrets.",
        "ANTHROPIC_API_KEY",
        "provider",
        "python",
        "uv",
        "skill",
        "id-token: write",
        "contents: write",
        "pull-requests: write",
    ] {
        assert!(
            !ordinary_ci
                .to_ascii_lowercase()
                .contains(&forbidden.to_ascii_lowercase()),
            "found {forbidden}"
        );
    }

    for entry in fs::read_dir(&workflows).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "yml") {
            let source = fs::read_to_string(&path).unwrap();
            assert!(!source.contains("actions/cache"), "{}", path.display());
            for line in source
                .lines()
                .filter(|line| line.trim().starts_with("uses:"))
            {
                let action_ref = line.split_once('@').unwrap().1.trim();
                assert_eq!(action_ref.len(), 40, "{}", path.display());
                assert!(
                    action_ref.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "{}",
                    path.display()
                );
            }
        }
    }

    for script in ["trusted-provider-gate.sh", "state-ref.sh"] {
        let source = fs::read_to_string(root().join("scripts/ci").join(script)).unwrap();
        assert!(source.starts_with("#!/bin/sh\n"));
        assert!(
            !source
                .lines()
                .any(|line| line.trim_start().starts_with("[[")),
            "{script} contains a non-POSIX conditional"
        );
        for non_posix in ["BASH_SOURCE", "pipefail", "local "] {
            assert!(!source.contains(non_posix), "{script} contains {non_posix}");
        }
    }
}

#[test]
fn state_ref_restores_publishes_and_rejects_stale_cas() {
    let fixture = GitFixture::new();
    let first_restore = state(&fixture.a, "restore", true);
    assert_eq!(
        String::from_utf8_lossy(&first_restore.stdout).trim(),
        ZERO_OID
    );
    initialize_database(&fixture.a);
    write_marker(&fixture.a.join(".fani/fani.db"), "initial");
    let first_publish = state(&fixture.a, "publish", true);
    let first_text = String::from_utf8_lossy(&first_publish.stdout);
    let first_oid = first_text.lines().last().unwrap();
    assert_eq!(first_oid.len(), 40);

    state(&fixture.a, "restore", true);
    state(&fixture.b, "restore", true);
    assert_eq!(read_marker(&fixture.b.join(".fani/fani.db")), "initial");
    write_marker(&fixture.a.join(".fani/fani.db"), "winner");
    write_marker(&fixture.b.join(".fani/fani.db"), "stale");
    state(&fixture.a, "publish", true);
    state(&fixture.b, "publish", false);

    let verifier = fixture.clone("verifier");
    state(&verifier, "restore", true);
    assert_eq!(read_marker(&verifier.join(".fani/fani.db")), "winner");
}

#[test]
fn state_ref_uses_fani_doctor_to_reject_corrupt_state() {
    let fixture = GitFixture::new();
    state(&fixture.a, "restore", true);
    initialize_database(&fixture.a);
    write_marker(&fixture.a.join(".fani/fani.db"), "valid");
    state(&fixture.a, "publish", true);

    let database = fixture.a.join(".fani/fani.db");
    fs::write(&database, b"not a SQLite database").unwrap();
    state(&fixture.a, "publish", false);

    let blob = String::from_utf8(
        git(
            &fixture.a,
            &["hash-object", "-w", database.to_str().unwrap()],
        )
        .stdout,
    )
    .unwrap();
    let tree_line = format!("100600 blob {}\tfani.db\n", blob.trim());
    let tree = String::from_utf8(git_input(&fixture.a, &["mktree"], &tree_line).stdout).unwrap();
    let remote_line = String::from_utf8(
        git(
            &fixture.a,
            &["ls-remote", "origin", "refs/heads/fani-state"],
        )
        .stdout,
    )
    .unwrap();
    let parent = remote_line.split_whitespace().next().unwrap();
    let commit = String::from_utf8(
        git(
            &fixture.a,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                parent,
                "-m",
                "corrupt state fixture",
            ],
        )
        .stdout,
    )
    .unwrap();
    git(
        &fixture.a,
        &[
            "push",
            "--force",
            "origin",
            &format!("{}:refs/heads/fani-state", commit.trim()),
        ],
    );

    let verifier = fixture.clone("corrupt-verifier");
    state(&verifier, "restore", false);
}
