use fani::test_support::db::Database;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn run(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fani(args: &[&str]) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .args(args)
        .output()
        .unwrap()
}

fn kill_sync_at_failpoint(config: &Path, reports: &Path, point: &str, marker: &Path) {
    let _ = fs::remove_file(marker);
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .args([
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ])
        .env("FANI_TEST_FAILPOINT", point)
        .env("FANI_TEST_FAILPOINT_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("fani exited before failpoint {point}: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "fani did not reach failpoint {point}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
}

#[test]
fn doctor_status_and_repeated_native_sync_need_no_external_skill() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(
        repo.join("docs/guide.md"),
        "# Hello `fani`\n\nWelcome, {{user}}. Read [the guide](https://example.invalid/guide).\n\n```sh\necho hello\n```\n",
    )
    .unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let counter = tmp.path().join("counter");
    let provider = tmp.path().join("provider.sh");
    fs::write(
        &provider,
        format!(
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\nawk '/^--- SOURCE ---$/ {{ take=1; next }} /^--- END SOURCE ---$/ {{ take=0 }} take' | sed -e 's/Hello/你好/g' -e 's/Welcome/欢迎/g' -e 's/Read/阅读/g' -e 's/the guide/指南/g'\n",
            counter = counter.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&provider).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&provider, permissions).unwrap();

    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
max_tasks = 20
repair_budget = 1
[repo.quality]
revision = false
proofread = false
[repo.publish]
enabled = false
source_ref = "HEAD"
[agents.fixture]
cmd = ["{}"]
concurrency = 2
timeout_s = 5
retries = 0
[routing]
translate = "fixture"
repair = "fixture"
"#,
            repo.display(),
            provider.display()
        ),
    )
    .unwrap();
    let reports = tmp.path().join("reports");

    let doctor = fani(&["doctor", "--config", config.to_str().unwrap()]);
    assert_eq!(
        doctor.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("native schema 1"));

    let status = fani(&["status", "--config", config.to_str().unwrap()]);
    assert_eq!(status.status.code(), Some(0));
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_text.contains("pending=2") && status_text.contains("source="),
        "{status_text}"
    );
    assert_eq!(
        status_text.matches("repo [zh-CN]").count(),
        1,
        "{status_text}"
    );
    assert!(!repo.join("translations/zh-CN/docs/guide.md").exists());

    let first = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stderr={} report={}",
        String::from_utf8_lossy(&first.stderr),
        fs::read_to_string(reports.join("report.json")).unwrap_or_default()
    );
    let target = repo.join("translations/zh-CN/docs/guide.md");
    let translated = fs::read_to_string(&target).unwrap();
    assert!(translated.contains("你好 `fani`") && translated.contains("{{user}}"));
    assert!(translated.contains("https://example.invalid/guide"));
    assert!(translated.contains("echo hello"));
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    let report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["totals"]["agent_calls"], 2);

    let second = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    let report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["totals"]["agent_calls"], 0);
    assert_eq!(report["totals"]["files_written"], 0);
    assert_eq!(
        report["languages"][0]["message"],
        "every translation is up to date"
    );
}

#[test]
fn unknown_config_field_fails_before_database_side_effect() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            "skill = '/tmp/old-skill'\n[[repo]]\npath = '{}'\nlanguages = ['zh-CN']\n[agents.fake]\ncmd = ['true']\n",
            repo.display()
        ),
    )
    .unwrap();
    let output = fani(&["doctor", "--config", config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("unknown field `skill`"));
    assert!(!repo.join(".fani/fani.db").exists());
}

#[test]
fn status_uses_a_temporary_snapshot_without_creating_repository_state() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "# Hello\n").unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
[repo.publish]
enabled = false
source_ref = "HEAD"
[agents.fixture]
cmd = ["true"]
[routing]
translate = "fixture"
repair = "fixture"
"#,
            repo.display()
        ),
    )
    .unwrap();

    let output = fani(&["status", "--config", config.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("pending=1"));
    assert!(!repo.join(".fani").exists());
}

#[test]
fn status_keeps_earlier_diagnostics_when_a_later_repository_errors() {
    let tmp = tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    for repo in [&first, &second] {
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("docs/guide.md"), "# Hello\n").unwrap();
        run(repo, &["init", "-q", "-b", "main"]);
        run(repo, &["config", "user.email", "test@example.invalid"]);
        run(repo, &["config", "user.name", "Test"]);
        run(repo, &["add", "."]);
        run(repo, &["commit", "-qm", "source"]);
    }

    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
target_pattern = "translations/{{lang}}/{{relpath}}"

[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
target_pattern = "translations/{{lang}}/{{relpath}}"
[repo.publish]
source_ref = "refs/heads/missing"

[agents.fake]
cmd = ["true"]
[routing]
translate = "fake"
"#,
            first.display(),
            second.display()
        ),
    )
    .unwrap();

    let output = fani(&["status", "--config", config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("first [zh-CN] source=") && stdout.contains("pending=1"),
        "stdout={stdout} stderr={stderr}"
    );
    assert_eq!(stdout.matches("first [zh-CN]").count(), 1, "{stdout}");
    assert!(
        stderr.contains("fani: ") && stderr.contains("missing"),
        "{stderr}"
    );
}

#[test]
fn adopt_promotes_human_translation_and_discard_restores_it() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "# Hello `fani`\n").unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let counter = tmp.path().join("counter");
    let provider = tmp.path().join("provider.sh");
    fs::write(
        &provider,
        format!(
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\nawk '/^--- SOURCE ---$/ {{ take=1; next }} /^--- END SOURCE ---$/ {{ take=0 }} take' | sed 's/Hello/你好/g'\n",
            counter = counter.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&provider).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&provider, permissions).unwrap();

    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
[repo.quality]
revision = false
proofread = false
[repo.publish]
enabled = false
source_ref = "HEAD"
[agents.fixture]
cmd = ["{}"]
timeout_s = 5
[routing]
translate = "fixture"
repair = "fixture"
"#,
            repo.display(),
            provider.display()
        ),
    )
    .unwrap();
    let reports = tmp.path().join("reports");
    let target = repo.join("translations/zh-CN/docs/guide.md");

    let initial = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(initial.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");

    let human_translation = "# 人工翻译 `fani`\n";
    fs::write(&target, human_translation).unwrap();
    let blocked = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        blocked.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&blocked.stdout),
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), human_translation);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    let blocked_report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(blocked_report["status"], "needs_human");
    assert_eq!(blocked_report["exit_code"], 1);
    assert!(
        blocked_report["languages"][0]["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["code"] == "HUMAN-EDIT")
    );

    let adopted = fani(&["adopt", "--config", config.to_str().unwrap()]);
    assert_eq!(
        adopted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&adopted.stderr)
    );

    let reused = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(reused.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    assert_eq!(fs::read_to_string(&target).unwrap(), "# 人工翻译 `fani`\n");

    fs::write(&target, "# 删除了受保护代码\n").unwrap();
    let rejected = fani(&["adopt", "--config", config.to_str().unwrap()]);
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("failed validation"));

    let discarded = fani(&["discard", "--config", config.to_str().unwrap()]);
    assert_eq!(discarded.status.code(), Some(0));
    assert_eq!(fs::read_to_string(&target).unwrap(), "# 人工翻译 `fani`\n");
}

#[test]
fn killed_sync_recovers_agent_materialization_and_publication_without_duplicate_provider_calls() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let remote = tmp.path().join("remote.git");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "# Hello\n").unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source one"]);
    run(
        tmp.path(),
        &["init", "--bare", "-q", remote.to_str().unwrap()],
    );
    run(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&repo, &["push", "-q", "-u", "origin", "main"]);

    let counter = tmp.path().join("counter");
    let provider = tmp.path().join("provider.sh");
    fs::write(
        &provider,
        format!(
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\nawk '/^--- SOURCE ---$/ {{ take=1; next }} /^--- END SOURCE ---$/ {{ take=0 }} take' | sed 's/Hello/你好/g'\n",
            counter = counter.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&provider).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&provider, permissions).unwrap();

    let config = tmp.path().join("fani.toml");
    let write_config = |publish: bool, revision: bool| {
        fs::write(
            &config,
            format!(
                r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
max_tasks = 20
repair_budget = 1
[repo.quality]
revision = {}
proofread = false
[repo.publish]
enabled = {}
source_ref = "HEAD"
branch = "i18n/{{lang}}"
push = {}
remote = "origin"
[repo.publish.github]
enabled = false
[agents.fixture]
cmd = ["{}"]
timeout_s = 5
retries = 0
[routing]
translate = "fixture"
repair = "fixture"
"#,
                repo.display(),
                revision,
                publish,
                publish,
                provider.display()
            ),
        )
        .unwrap();
    };
    write_config(false, false);
    let reports = tmp.path().join("reports");
    let marker = tmp.path().join("failpoint-marker");
    let target = repo.join("translations/zh-CN/docs/guide.md");

    kill_sync_at_failpoint(&config, &reports, "agent_candidate_committed", &marker);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    let database = Database::open(repo.join(".fani/fani.db")).unwrap();
    let conn = database.connect().unwrap();
    let durable: (i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM attempts WHERE status='succeeded'),(SELECT COUNT(*) FROM canonical_candidates WHERE selected=1)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(durable, (1, 1));
    drop(conn);

    let recovered_agent = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        recovered_agent.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&recovered_agent.stderr)
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    assert_eq!(fs::read_to_string(&target).unwrap(), "# 你好\n");

    fs::write(repo.join("docs/guide.md"), "# Hello again\n").unwrap();
    run(&repo, &["add", "docs/guide.md"]);
    run(&repo, &["commit", "-qm", "source two"]);
    kill_sync_at_failpoint(&config, &reports, "materialized_file_written", &marker);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    assert_eq!(fs::read_to_string(&target).unwrap(), "# 你好 again\n");

    let recovered_materialization = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        recovered_materialization.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&recovered_materialization.stderr)
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    let conn = database.connect().unwrap();
    let incomplete_materializations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM materialization_outbox WHERE state!='done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(incomplete_materializations, 0);
    drop(conn);

    write_config(true, false);
    fs::write(repo.join("docs/guide.md"), "# Hello publication\n").unwrap();
    run(&repo, &["add", "docs/guide.md"]);
    run(&repo, &["commit", "-qm", "source three"]);
    kill_sync_at_failpoint(
        &config,
        &reports,
        "publication_side_effect_completed",
        &marker,
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "3");
    let remote_before = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/i18n/zh-CN",
        ])
        .output()
        .unwrap();
    assert!(remote_before.status.success());
    let remote_before = String::from_utf8(remote_before.stdout).unwrap();
    let conn = database.connect().unwrap();
    let durable_publication_commit: String = conn
        .query_row(
            "SELECT json_extract(payload_json,'$.commit') FROM publication_outbox WHERE state='processing' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remote_before.trim(), durable_publication_commit);
    drop(conn);
    fs::write(repo.join("notes.txt"), "unrelated source advance\n").unwrap();
    run(&repo, &["add", "notes.txt"]);
    run(
        &repo,
        &["commit", "-qm", "advance source after publication crash"],
    );
    thread::sleep(Duration::from_millis(1_100));

    let recovered_publication = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        recovered_publication.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&recovered_publication.stderr)
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "3");
    let remote_after = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/i18n/zh-CN",
        ])
        .output()
        .unwrap();
    assert!(remote_after.status.success());
    assert_eq!(
        String::from_utf8(remote_after.stdout).unwrap(),
        remote_before,
        "publication replay replaced an already-pushed candidate commit"
    );
    let conn = database.connect().unwrap();
    let publication_state: (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*),SUM(CASE WHEN state='done' THEN 1 ELSE 0 END) FROM publication_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(publication_state.0, publication_state.1);
    assert!(publication_state.0 >= 1);
    drop(conn);

    write_config(false, true);
    fs::write(repo.join("docs/guide.md"), "# Hello revision\n").unwrap();
    run(&repo, &["add", "docs/guide.md"]);
    run(&repo, &["commit", "-qm", "source four"]);
    kill_sync_at_failpoint(&config, &reports, "revision_candidate_committed", &marker);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "5");

    let recovered_revision = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        recovered_revision.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&recovered_revision.stderr)
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "5");
    let conn = database.connect().unwrap();
    let durable_revision_attempts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM attempts WHERE status='succeeded' AND dedupe_key LIKE '%:revision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(durable_revision_attempts, 1);
    database.integrity_check().unwrap();
}
