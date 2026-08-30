use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
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

    fs::write(&target, "# 人工翻译 `fani`\n").unwrap();
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
