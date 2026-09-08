use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn repository(path: &Path) {
    fs::create_dir_all(path.join("docs")).unwrap();
    fs::write(path.join("docs/example.md"), "```txt\npass through\n```\n").unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
        vec!["add", "docs"],
        vec!["commit", "-qm", "source"],
    ] {
        let result = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn database_failure_is_reported_and_later_repository_still_runs() {
    let tmp = tempdir().unwrap();
    let broken = tmp.path().join("broken");
    let healthy = tmp.path().join("healthy");
    repository(&broken);
    repository(&healthy);
    fs::create_dir(broken.join(".fani")).unwrap();
    fs::write(broken.join(".fani/fani.db"), "invalid SQLite database").unwrap();
    let mut config = String::new();
    for repo in [&broken, &healthy] {
        config.push_str(&format!(
            r#"[[repo]]
path = {:?}
languages = ["zh-CN", "fr"]
include = ["docs/**/*.md"]
target_pattern = "i18n/{{lang}}/{{relpath}}"
[repo.quality]
revision = false
proofread = false
[repo.publish]
enabled = false
source_ref = "HEAD"
"#,
            repo.to_str().unwrap()
        ));
    }
    config.push_str(
        r#"
[agents.fixture]
provider = "fixture"
model = "fixture"
adapter = "command-json-v1"
cmd = ["/bin/false"]
[routing]
translate = "fixture"
repair = "fixture"
"#,
    );
    let config_path = tmp.path().join("fani.toml");
    fs::write(&config_path, config).unwrap();
    let reports = tmp.path().join("reports");
    let result = Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .args(["sync", "--quiet", "--config"])
        .arg(config_path)
        .arg("--report-dir")
        .arg(&reports)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(reports.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["status"], "error");
    let outcomes = report["languages"].as_array().unwrap();
    assert_eq!(outcomes.len(), 4);
    for outcome in &outcomes[..2] {
        assert_eq!(outcome["status"], "error");
        assert!(outcome["message"].as_str().unwrap().contains("database"));
        assert_eq!(outcome["repo"], broken.to_str().unwrap());
    }
    for outcome in &outcomes[2..] {
        assert_eq!(outcome["status"], "ok");
    }
    assert_eq!(report["totals"]["agent_calls"], 0);
    for language in ["zh-CN", "fr"] {
        assert_eq!(
            fs::read(healthy.join(format!("i18n/{language}/docs/example.md"))).unwrap(),
            fs::read(healthy.join("docs/example.md")).unwrap()
        );
    }
    assert_eq!(
        fs::read(broken.join(".fani/fani.db")).unwrap(),
        b"invalid SQLite database"
    );
    assert!(reports.join("report.md").is_file());
}
