use fani::test_support::db::Database;
use serde_json::Value;
use sha2::Digest;
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

#[test]
fn init_creates_a_script_free_safe_starter_config() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let config = tmp.path().join("fani.toml");
    let output = fani(&[
        "init",
        "--config",
        config.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--lang",
        "zh-CN",
        "--provider",
        "anthropic",
        "--model",
        "claude-test",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("provider = \"anthropic\""));
    assert!(text.contains("model = \"claude-test\""));
    assert!(text.contains(&format!(
        "path = {}",
        serde_json::to_string(&fs::canonicalize(&repo).unwrap().to_string_lossy()).unwrap()
    )));
    assert!(text.contains("enabled = false"));
    assert!(!text.contains("cmd ="));
    assert!(!text.contains("adapter ="));

    let second = fani(&[
        "init",
        "--config",
        config.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--lang",
        "zh-CN",
        "--provider",
        "anthropic",
        "--model",
        "claude-test",
    ]);
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already exists"));
}

fn strict_provider_case(script: &str, output_file: bool) -> (i32, Value) {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "# Hello\n").unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let provider = tmp.path().join("provider.sh");
    fs::write(&provider, script).unwrap();
    let mut permissions = fs::metadata(&provider).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&provider, permissions).unwrap();
    let command = if output_file {
        format!("cmd = [\"{}\", \"{{output_file}}\"]", provider.display())
    } else {
        format!("cmd = [\"{}\"]", provider.display())
    };
    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
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
{}
timeout_s = 5
retries = 0
[routing]
translate = "fixture"
"#,
            repo.display(),
            command
        ),
    )
    .unwrap();
    let reports = tmp.path().join("reports");
    let output = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    let report =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    (output.status.code().unwrap(), report)
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
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(.task.source | gsub(\"Hello\";\"你好\") | gsub(\"Welcome\";\"欢迎\") | gsub(\"Read\";\"阅读\") | gsub(\"the guide\";\"指南\"))}}'\n",
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
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
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
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("native schema 2"));

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
    let database = Database::open(repo.join(".fani/fani.db")).unwrap();
    let provenance: (String, String, String, String, String, String, String, String) = database
        .connect()
        .unwrap()
        .query_row(
            "SELECT provider,model,adapter,provider_fingerprint,prompt_version,prompt_hash,policy_fingerprint,request_json FROM attempts LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(provenance.0, "fixture-provider");
    assert_eq!(provenance.1, "fixture-model");
    assert_eq!(provenance.2, "command-json-v1");
    assert_eq!(provenance.3.len(), 64);
    assert_eq!(provenance.4, fani::domain::prompts::PROMPT_VERSION);
    assert_eq!(provenance.5.len(), 64);
    assert_eq!(provenance.6, fani::domain::prompts::policy_fingerprint());
    let durable_request: Value = serde_json::from_str(&provenance.7).unwrap();
    assert_eq!(durable_request["schema"], "fani.agent.request.v1");
    assert!(durable_request["prompt"]["content"].is_string());
    assert!(!report.to_string().contains("--- SOURCE ---"));

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
fn bounded_sync_resumes_candidates_without_false_duplicate_ambiguity() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(
        repo.join("docs/guide.md"),
        "# Alpha\n\nRepeated.\n\nRepeated.\n\nFinal.\n",
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
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\njq -c --arg count \"$count\" '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(if .task.source == \"Repeated.\" then (\"重复\" + $count + \".\") else (.task.source | gsub(\"Alpha\";\"阿尔法\") | gsub(\"Final\";\"最后\")) end)}}'\n",
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
max_tasks = 2
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
cmd = ["{}"]
concurrency = 1
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

    let first = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    let first_report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(
        first.status.code(),
        Some(3),
        "stdout={} stderr={} report={first_report}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(first_report["status"], "partial");
    assert_eq!(first_report["totals"]["findings"], 0);
    assert_eq!(first_report["totals"]["remaining_tasks"], 2);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    assert!(!repo.join("translations/zh-CN/docs/guide.md").exists());

    fs::write(repo.join("unrelated.txt"), "new repository revision\n").unwrap();
    run(&repo, &["add", "unrelated.txt"]);
    run(&repo, &["commit", "-qm", "change unrelated file"]);

    let status = fani(&["status", "--config", config.to_str().unwrap()]);
    assert_eq!(
        status.status.code(),
        Some(3),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(status_text.contains("conflicts=0"), "{status_text}");

    let second = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    let second_report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(
        second.status.code(),
        Some(0),
        "stdout={} stderr={} report={second_report}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(second_report["status"], "ok");
    assert_eq!(second_report["totals"]["agent_calls"], 2);
    assert_eq!(second_report["totals"]["conflicts"], 0);
    assert_eq!(second_report["totals"]["findings"], 0);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "4");
    assert_eq!(
        fs::read_to_string(repo.join("translations/zh-CN/docs/guide.md")).unwrap(),
        "# 阿尔法\n\n重复2.\n\n重复3.\n\n最后.\n"
    );

    let third = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    let third_report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(third.status.code(), Some(0), "{third_report}");
    assert_eq!(third_report["totals"]["agent_calls"], 0);
    assert_eq!(third_report["totals"]["files_written"], 0);
    assert_eq!(
        third_report["languages"][0]["message"],
        "every translation is up to date"
    );
    assert_eq!(fs::read_to_string(&counter).unwrap(), "4");

    fs::write(
        repo.join("docs/guide.md"),
        "# Alpha\n\nRepeated.\n\nFinal.\n",
    )
    .unwrap();
    run(&repo, &["add", "docs/guide.md"]);
    run(&repo, &["commit", "-qm", "delete ambiguous duplicate"]);
    for attempt in 1..=2 {
        let changed = fani(&[
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ]);
        let changed_report: Value =
            serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(
            changed.status.code(),
            Some(1),
            "attempt={attempt} stdout={} stderr={} report={changed_report}",
            String::from_utf8_lossy(&changed.stdout),
            String::from_utf8_lossy(&changed.stderr)
        );
        assert_eq!(changed_report["totals"]["conflicts"], 1);
    }
    assert_eq!(fs::read_to_string(&counter).unwrap(), "4");
}

#[test]
fn multi_language_status_keeps_duplicate_identity_after_unrelated_revision() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "Repeated.\n\nRepeated.\n").unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let provider = tmp.path().join("provider.sh");
    fs::write(
        &provider,
        "#!/bin/sh\nset -eu\njq -c '{schema:\"fani.agent.response.v1\",task_id:.task.id,output:\"译文.\"}'\n",
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
languages = ["zh-CN", "ja"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
max_tasks = 2
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
cmd = ["{}"]
concurrency = 1
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

    let initial = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    assert_eq!(
        initial.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&initial.stdout),
        String::from_utf8_lossy(&initial.stderr)
    );

    fs::write(repo.join("unrelated.txt"), "new repository revision\n").unwrap();
    run(&repo, &["add", "unrelated.txt"]);
    run(&repo, &["commit", "-qm", "change unrelated file"]);

    let status = fani(&["status", "--config", config.to_str().unwrap()]);
    assert_ne!(
        status.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert_eq!(
        status_text.matches("conflicts=0").count(),
        2,
        "{status_text}"
    );
}

#[test]
fn repair_receives_rejected_output_and_exact_validation_findings() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(
        repo.join("docs/guide.md"),
        "See [label](https://example.com).\n",
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
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\nif [ \"$count\" -eq 1 ]; then\n  jq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:\"See label.\"}}'\nelse\n  jq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(if .task.previous_translation == \"See label.\" and any(.task.findings[]; .code == \"MD-PROTECTED\") then .task.source else \"still invalid\" end)}}'\nfi\n",
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
max_tasks = 1
repair_budget = 1
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
cmd = ["{}"]
concurrency = 1
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

    let output = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    let report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={} report={report}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["totals"]["agent_calls"], 2);
    assert_eq!(report["totals"]["repair_rounds"], 1);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
    assert_eq!(
        fs::read_to_string(repo.join("translations/zh-CN/docs/guide.md")).unwrap(),
        "See [label](https://example.com).\n"
    );
}

#[test]
fn missing_separator_after_leading_strong_is_repaired_deterministically() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "**Label:** text.\n").unwrap();
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
            "#!/bin/sh\nset -eu\nprintf 1 > '{counter}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:\"**标签：**文本。\"}}'\n",
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
max_tasks = 1
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
cmd = ["{}"]
concurrency = 1
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

    let output = fani(&[
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--report-dir",
        reports.to_str().unwrap(),
        "--quiet",
    ]);
    let report: Value =
        serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report}");
    assert_eq!(report["totals"]["agent_calls"], 1);
    assert_eq!(report["totals"]["repair_rounds"], 0);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    assert_eq!(
        fs::read_to_string(repo.join("translations/zh-CN/docs/guide.md")).unwrap(),
        "**标签：** 文本。\n"
    );
}

#[test]
fn failed_scheduled_duplicate_remains_blocking_on_resume() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "Repeated.\n\nRepeated.\n").unwrap();
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
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\njq -c --arg count \"$count\" '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(if $count == \"1\" then \"重复.\" else \"# broken\" end)}}'\n",
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
max_tasks = 2
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
cmd = ["{}"]
concurrency = 1
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

    for attempt in 1..=2 {
        let output = fani(&[
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ]);
        let report: Value =
            serde_json::from_str(&fs::read_to_string(reports.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "attempt={attempt} stdout={} stderr={} report={report}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(report["status"], "needs_human");
        assert_eq!(report["totals"]["findings"], 1);
        assert_eq!(
            report["languages"][0]["findings"][0]["code"],
            "VERIFY-FAILED"
        );
        assert!(!repo.join("translations/zh-CN/docs/guide.md").exists());
    }
    assert_eq!(fs::read_to_string(&counter).unwrap(), "2");
}

#[test]
fn command_adapter_accepts_only_strict_versioned_json_envelopes() {
    let (code, report) = strict_provider_case(
        r##"#!/bin/sh
set -eu
input=$(cat)
printf '%s' "$input" | jq -c '{schema:"fani.agent.response.v1",task_id:.task.id,output:(.task.source | gsub("Hello";"你好"))}' > "$1"
"##,
        true,
    );
    assert_eq!(code, 0, "{report}");

    let invalid = [
        r##"#!/bin/sh
jq -c '{schema:"fani.agent.response.v2",task_id:.task.id,output:"# 你好"}'
"##,
        r##"#!/bin/sh
jq -c '{schema:"fani.agent.response.v1",task_id:.task.id,output:"# 你好",extra:true}'
"##,
        r##"#!/bin/sh
jq -c '{schema:"fani.agent.response.v1",task_id:.task.id}'
"##,
        r##"#!/bin/sh
jq -c '{schema:"fani.agent.response.v1",task_id:.task.id,output:42}'
"##,
        r##"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"schema":"fani.agent.response.v1","task_id":"wrong-task","output":"# 你好"}'
"##,
        r##"#!/bin/sh
cat >/dev/null
printf '%s\n' '# raw output'
"##,
        r##"#!/bin/sh
cat >/dev/null
printf '%s\n' '```json' '{"schema":"fani.agent.response.v1","task_id":"ignored","output":"# 你好"}' '```'
"##,
    ];
    for script in invalid {
        let (code, report) = strict_provider_case(script, false);
        assert_ne!(code, 0, "{report}");
        assert_eq!(
            report["languages"][0]["agent_calls"][0]["code"], "AGENT-INVALID",
            "{report}"
        );
    }

    let (code, report) = strict_provider_case(
        r##"#!/bin/sh
set -eu
cat >/dev/null
printf '%s' '# raw output' > "$1"
"##,
        true,
    );
    assert_ne!(code, 0, "{report}");
    assert_eq!(
        report["languages"][0]["agent_calls"][0]["code"],
        "AGENT-INVALID"
    );

    let (code, report) = strict_provider_case(
        r##"#!/bin/sh
set -eu
cat >/dev/null
printf '%s' '{"schema":"fani.agent.response.v1","task_id":"task","output":"' > "$1"
head -c 4194305 /dev/zero | tr '\000' a >> "$1"
printf '%s' '"}' >> "$1"
"##,
        true,
    );
    assert_ne!(code, 0, "{report}");
    assert_eq!(
        report["languages"][0]["agent_calls"][0]["code"],
        "AGENT-INVALID"
    );
    assert!(
        report["languages"][0]["agent_calls"][0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("exceeded")
    );

    let (_, report) = strict_provider_case(
        r##"#!/bin/sh
cat >/dev/null
printf '%s\n' 'secret-token full prompt content' >&2
exit 1
"##,
        false,
    );
    let ordinary_diagnostics = report.to_string();
    assert!(!ordinary_diagnostics.contains("secret-token"));
    assert!(!ordinary_diagnostics.contains("full prompt content"));
    assert!(ordinary_diagnostics.contains("content redacted"));
}

fn adversarial_diagnostics_case(json: bool) -> Output {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(
        repo.join("docs/guide.md"),
        "# SOURCE_SECRET_DO_NOT_LOG\n\nCREDENTIAL_SOURCE_DO_NOT_LOG {{TOKEN_PLACEHOLDER_SECRET_DO_NOT_LOG}}\n",
    )
    .unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let provider = tmp.path().join("provider.sh");
    fs::write(
        &provider,
        r##"#!/bin/sh
set -eu
printf '%s\n' 'PROVIDER_STDERR_SECRET_DO_NOT_LOG' >&2
jq -c '{schema:"fani.agent.response.v1",task_id:.task.id,output:(.task.source | gsub("SOURCE_SECRET_DO_NOT_LOG";"TRANSLATION_SECRET_DO_NOT_LOG") | gsub("CREDENTIAL_SOURCE_DO_NOT_LOG";"TRANSLATED_CREDENTIAL_DO_NOT_LOG"))}'
"##,
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
target_pattern = "translations/{{lang}}/{{relpath}}"
repair_budget = 0
[repo.quality]
revision = false
proofread = false
[repo.publish]
enabled = false
source_ref = "HEAD"
[agents.fixture]
provider = "PROVIDER_METADATA_SECRET_DO_NOT_LOG"
model = "MODEL_CREDENTIAL_SECRET_DO_NOT_LOG"
adapter = "command-json-v1"
cmd = ["{}"]
timeout_s = 5
retries = 0
env_allow = ["FANI_TEST_DIAGNOSTIC_TOKEN"]
[routing]
translate = "fixture"
"#,
            repo.display(),
            provider.display(),
        ),
    )
    .unwrap();
    let reports = tmp.path().join("reports");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("fani"));
    command
        .args([
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ])
        .env("FANI_LOG", "info")
        .env("FANI_TEST_DIAGNOSTIC_TOKEN", "ENV_TOKEN_SECRET_DO_NOT_LOG");
    if json {
        command.env("FANI_LOG_FORMAT", "json");
    } else {
        command.env_remove("FANI_LOG_FORMAT");
    }
    command.output().unwrap()
}

fn assert_diagnostics_are_redacted(output: &Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stdout.is_empty(),
        "quiet sync changed stdout semantics: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let diagnostics = String::from_utf8(output.stderr.clone()).unwrap();
    for secret in [
        "SOURCE_SECRET_DO_NOT_LOG",
        "CREDENTIAL_SOURCE_DO_NOT_LOG",
        "TOKEN_PLACEHOLDER_SECRET_DO_NOT_LOG",
        "TRANSLATION_SECRET_DO_NOT_LOG",
        "TRANSLATED_CREDENTIAL_DO_NOT_LOG",
        "PROVIDER_STDERR_SECRET_DO_NOT_LOG",
        "PROVIDER_METADATA_SECRET_DO_NOT_LOG",
        "MODEL_CREDENTIAL_SECRET_DO_NOT_LOG",
        "ENV_TOKEN_SECRET_DO_NOT_LOG",
    ] {
        assert!(
            !diagnostics.contains(secret),
            "diagnostics leaked {secret}: {diagnostics}"
        );
    }
    diagnostics
}

#[test]
fn human_diagnostics_are_structured_and_redacted() {
    let diagnostics = assert_diagnostics_are_redacted(&adversarial_diagnostics_case(false));
    for event in [
        "cli.command.started",
        "run.started",
        "stage.completed",
        "provider.batch.completed",
        "outbox.completed",
        "run.completed",
    ] {
        assert!(
            diagnostics.contains(event),
            "missing {event}: {diagnostics}"
        );
    }
    assert!(diagnostics.contains("duration_ms"), "{diagnostics}");
    assert!(diagnostics.contains("locale=\"zh-CN\""), "{diagnostics}");
}

#[test]
fn json_diagnostics_are_structured_and_redacted() {
    let diagnostics = assert_diagnostics_are_redacted(&adversarial_diagnostics_case(true));
    let records = diagnostics
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("invalid JSON diagnostic {line:?}: {error}"))
        })
        .collect::<Vec<_>>();
    assert!(!records.is_empty());
    let events = records
        .iter()
        .filter_map(|record| record["fields"]["event"].as_str())
        .collect::<Vec<_>>();
    for event in [
        "cli.command.started",
        "run.started",
        "stage.completed",
        "provider.batch.completed",
        "outbox.completed",
        "run.completed",
    ] {
        assert!(events.contains(&event), "missing {event}: {diagnostics}");
    }
    assert!(records.iter().any(|record| {
        record["fields"]["duration_ms"].is_number() && record["fields"]["status"].is_string()
    }));
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
            "skill = '/tmp/old-skill'\n[[repo]]\npath = '{}'\nlanguages = ['zh-CN']\n[agents.fake]\nprovider = 'fixture-provider'\nmodel = 'fixture-model'\nadapter = 'command-json-v1'\ncmd = ['true']\n",
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
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
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
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
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
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(.task.source | gsub(\"Hello\";\"你好\"))}}'\n",
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
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
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
    let database = Database::open(repo.join(".fani/fani.db")).unwrap();
    let canonical_content: Vec<u8> = database
        .connect()
        .unwrap()
        .query_row(
            "SELECT content FROM canonical_files WHERE locale='zh-CN' AND path='translations/zh-CN/docs/guide.md'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(canonical_content, human_translation.as_bytes());

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
            "#!/bin/sh\nset -eu\ncount=0\n[ ! -f '{counter}' ] || count=$(cat '{counter}')\ncount=$((count+1))\nprintf '%s' \"$count\" > '{counter}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(.task.source | gsub(\"Hello\";\"你好\"))}}'\n",
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
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
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
    let unrelated_candidates: i64 = conn
        .query_row(
            r#"SELECT COUNT(*)
               FROM translation_memory_entries tm
               WHERE tm.tier='candidate' AND tm.locale='zh-CN'
                 AND NOT EXISTS (
                     SELECT 1
                     FROM publication_manifests pm
                     JOIN publication_manifest_files pmf ON pmf.manifest_id=pm.id
                     JOIN canonical_file_translations cft
                       ON cft.canonical_content_version_id=pmf.canonical_content_version_id
                     WHERE pm.candidate_commit=?1
                       AND cft.translation_version_id=tm.translation_version_id
                 )"#,
            [&durable_publication_commit],
            |row| row.get(0),
        )
        .unwrap();
    assert!(unrelated_candidates >= 1);
    drop(conn);
    assert!(
        database
            .promote_merged_publication(
                database
                    .connect()
                    .unwrap()
                    .query_row("SELECT id FROM repositories LIMIT 1", [], |row| row.get(0))
                    .unwrap(),
                "zh-CN",
                &durable_publication_commit,
                "github_merged",
            )
            .unwrap()
            >= 1
    );
    let conn = database.connect().unwrap();
    let trusted_unrelated: i64 = conn
        .query_row(
            r#"SELECT COUNT(*)
               FROM translation_memory_entries trusted
               WHERE trusted.tier='trusted' AND trusted.locale='zh-CN'
                 AND EXISTS (
                     SELECT 1 FROM translation_memory_entries candidate
                     WHERE candidate.tier='candidate'
                       AND candidate.source_hash=trusted.source_hash
                       AND candidate.locale=trusted.locale
                       AND NOT EXISTS (
                           SELECT 1
                           FROM publication_manifests pm
                           JOIN publication_manifest_files pmf ON pmf.manifest_id=pm.id
                           JOIN canonical_file_translations cft
                             ON cft.canonical_content_version_id=pmf.canonical_content_version_id
                           WHERE pm.candidate_commit=?1
                             AND cft.translation_version_id=candidate.translation_version_id
                       )
                 )"#,
            [&durable_publication_commit],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trusted_unrelated, 0);
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

struct DocumentationCheckCase {
    _tmp: tempfile::TempDir,
    repo: std::path::PathBuf,
    config: std::path::PathBuf,
    reports: std::path::PathBuf,
    capture: std::path::PathBuf,
}

fn documentation_check_case(checker_body: &str, timeout_s: f64) -> DocumentationCheckCase {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/guide.md"), "# Hello\n").unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "test@example.invalid"]);
    run(&repo, &["config", "user.name", "Test"]);
    run(&repo, &["add", "."]);
    run(&repo, &["commit", "-qm", "source"]);

    let provider = tmp.path().join("provider.sh");
    fs::write(
        &provider,
        r##"#!/bin/sh
set -eu
jq -c '{schema:"fani.agent.response.v1",task_id:.task.id,output:(.task.source | gsub("Hello";"你好"))}'
"##,
    )
    .unwrap();
    let checker = tmp.path().join("checker.sh");
    fs::write(&checker, format!("#!/bin/sh\nset -eu\n{checker_body}\n")).unwrap();
    for executable in [&provider, &checker] {
        let mut permissions = fs::metadata(executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }
    let capture = tmp.path().join("checked.md");
    let config = tmp.path().join("fani.toml");
    fs::write(
        &config,
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
target_pattern = "translations/{{lang}}/{{relpath}}"
repair_budget = 0
[repo.quality]
revision = false
proofread = false
[repo.documentation]
commands = [["{}", "{}"]]
timeout_s = {}
[repo.publish]
enabled = false
source_ref = "HEAD"
[agents.fixture]
provider = "fixture-provider"
model = "fixture-model"
adapter = "command-json-v1"
cmd = ["{}"]
timeout_s = 5
retries = 0
env_allow = ["FANI_TEST_PROVIDER_SECRET"]
[routing]
translate = "fixture"
"#,
            repo.display(),
            checker.display(),
            capture.display(),
            timeout_s,
            provider.display(),
        ),
    )
    .unwrap();
    let reports = tmp.path().join("reports");
    DocumentationCheckCase {
        _tmp: tmp,
        repo,
        config,
        reports,
        capture,
    }
}

fn run_documentation_case(case: &DocumentationCheckCase) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .args([
            "sync",
            "--config",
            case.config.to_str().unwrap(),
            "--report-dir",
            case.reports.to_str().unwrap(),
            "--quiet",
        ])
        .env("FANI_TEST_PROVIDER_SECRET", "provider-only-secret")
        .output()
        .unwrap()
}

#[test]
fn documentation_check_runs_successfully_without_provider_environment() {
    let case = documentation_check_case(
        r#"[ -z "${FANI_TEST_PROVIDER_SECRET:-}" ]
[ "${FANI_DOCUMENTATION_CHECK:-}" = "1" ]
test -f translations/zh-CN/docs/guide.md
cp translations/zh-CN/docs/guide.md "$1""#,
        5.0,
    );
    let output = run_documentation_case(&case);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&case.capture).unwrap(),
        b"# \xe4\xbd\xa0\xe5\xa5\xbd\n"
    );
}

#[test]
fn documentation_check_receives_exact_candidate_bytes() {
    let case = documentation_check_case(
        r#"sha256sum translations/zh-CN/docs/guide.md | cut -d' ' -f1 > "$1""#,
        5.0,
    );
    let output = run_documentation_case(&case);
    assert_eq!(output.status.code(), Some(0));
    let candidate = fs::read(case.repo.join("translations/zh-CN/docs/guide.md")).unwrap();
    let expected = format!("{:x}\n", sha2::Sha256::digest(&candidate));
    assert_eq!(fs::read_to_string(&case.capture).unwrap(), expected);
    assert_eq!(candidate, b"# \xe4\xbd\xa0\xe5\xa5\xbd\n");
}

#[test]
fn nonzero_documentation_check_is_a_persisted_blocking_finding() {
    let case = documentation_check_case("printf 'bad docs' >&2\nexit 7", 5.0);
    let output = run_documentation_case(&case);
    assert_eq!(output.status.code(), Some(1));
    let report: Value =
        serde_json::from_str(&fs::read_to_string(case.reports.join("report.json")).unwrap())
            .unwrap();
    assert_eq!(
        report["languages"][0]["findings"][0]["code"],
        "DOC-CHECK-FAILED"
    );
    assert!(
        report["languages"][0]["published"]["commit"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let database = Database::open(case.repo.join(".fani/fani.db")).unwrap();
    let persisted: i64 = database
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM findings WHERE code='DOC-CHECK-FAILED' AND severity='error'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, 1);
}

#[test]
fn timed_out_documentation_check_is_a_blocking_finding() {
    let case = documentation_check_case("sleep 5", 0.05);
    let started = Instant::now();
    let output = run_documentation_case(&case);
    assert_eq!(output.status.code(), Some(1));
    assert!(started.elapsed() < Duration::from_secs(2));
    let report: Value =
        serde_json::from_str(&fs::read_to_string(case.reports.join("report.json")).unwrap())
            .unwrap();
    assert_eq!(
        report["languages"][0]["findings"][0]["code"],
        "DOC-CHECK-TIMEOUT"
    );
}

#[test]
fn unknown_documentation_check_config_fails_before_side_effects() {
    let case = documentation_check_case("true", 5.0);
    let text = fs::read_to_string(&case.config)
        .unwrap()
        .replace("commands =", "command =");
    fs::write(&case.config, text).unwrap();
    let output = fani(&["doctor", "--config", case.config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("unknown field `command`"));
    assert!(!case.repo.join(".fani/fani.db").exists());
}
