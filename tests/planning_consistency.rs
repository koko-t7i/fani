use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    repo: PathBuf,
    config: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("docs/guide.md"), source).unwrap();
        let config = temp.path().join("fani.toml");
        let target = repo.join("translations/zh-CN/docs/guide.md");
        let fixture = Self {
            temp,
            repo,
            config,
            target,
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.email", "test@example.invalid"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.commit();
        let provider = fixture.temp.path().join("provider.sh");
        fs::write(&provider, format!("#!/bin/sh\nset -eu\nprintf 'call\\n' >> '{}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(.task.source | gsub(\"Hello\";\"Bonjour\"))}}'\n", fixture.temp.path().join("calls").display())).unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            &fixture.config,
            format!(
                r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
include = ["docs/**/*.md"]
data_dir = ".fani"
target_pattern = "translations/{{lang}}/{{relpath}}"
max_tasks = 10
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
timeout_s = 5
retries = 0
[routing]
translate = "fixture"
"#,
                fixture.repo.display(),
                provider.display()
            ),
        )
        .unwrap();
        fixture
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit(&self) {
        self.git(&["add", "docs"]);
        self.git(&["commit", "-qm", "source"]);
    }

    fn run(&self, command: &str) -> Output {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("fani"));
        let bin = self.temp.path().join("bin");
        if bin.is_dir() {
            cmd.env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            );
        }
        cmd.args([command, "--config"]).arg(&self.config);
        if command == "sync" {
            cmd.arg("--report-dir")
                .arg(self.temp.path().join("reports"))
                .arg("--quiet");
        }
        cmd.output().unwrap()
    }

    fn sync(&self, expected: i32) {
        let output = self.run("sync");
        assert_eq!(
            output.status.code(),
            Some(expected),
            "stdout={} stderr={} report={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(self.temp.path().join("reports/report.json")).unwrap_or_default()
        );
        if expected == 3 {
            let report: Value = serde_json::from_str(
                &fs::read_to_string(self.temp.path().join("reports/report.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(report["status"], "partial");
        }
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.temp.path().join("calls")).unwrap_or_default()
    }

    fn preview(&self, expected: i32, conflicts: usize) {
        let before_calls = self.calls();
        let db = self.repo.join(".fani/fani.db");
        let before_db = fs::read(&db).ok();
        for command in ["status", "check"] {
            let output = self.run(command);
            assert_eq!(
                output.status.code(),
                Some(expected),
                "{command}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains(&format!("conflicts={conflicts}"))
            );
            assert_eq!(self.calls(), before_calls, "preview dispatched an Agent");
            assert_eq!(
                fs::read(&db).ok(),
                before_db,
                "preview changed authoritative state"
            );
        }
    }
}

fn fixture(source: &str, json: bool) -> Fixture {
    let mut fixture = Fixture::new(if json { "" } else { source });
    if json {
        fs::remove_file(fixture.repo.join("docs/guide.md")).unwrap();
        fs::write(fixture.repo.join("docs/guide.json"), source).unwrap();
        fixture.commit();
        let config = fs::read_to_string(&fixture.config).unwrap()
            .replace("include = [\"docs/**/*.md\"]", "sources = [{ format = 'json', message_syntax = 'i18next-interpolation-v1', include = ['docs/*.json'], strip_prefix = 'docs/', target_pattern = 'translations/{lang}/{relpath}' }]")
            .replace("target_pattern = \"translations/{lang}/{relpath}\"\n", "");
        fs::write(&fixture.config, config).unwrap();
        fixture.target = fixture.repo.join("translations/zh-CN/guide.json");
    }
    fixture
}

#[test]
fn canonical_target_edits_and_deletions_match_execution_for_all_document_shapes() {
    for (json, source) in [
        (false, "Hello world.\n"),
        (false, "<!-- opaque -->\n"),
        (true, r#"{"message":"Hello world."}"#),
        (true, r#"{"empty":"","scalar":1}"#),
    ] {
        let fixture = fixture(source, json);
        fixture.preview(0, 0);
        fixture.sync(0);
        fixture.preview(0, 0);
        let canonical = fs::read(&fixture.target).unwrap();
        let calls = fixture.calls();
        for delete in [false, true] {
            if delete {
                fs::remove_file(&fixture.target).unwrap();
            } else {
                fs::write(&fixture.target, b"human content").unwrap();
            }
            fixture.preview(1, 1);
            fixture.sync(1);
            assert_eq!(fixture.calls(), calls);
            let report: Value = serde_json::from_slice(
                &fs::read(fixture.temp.path().join("reports/report.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(report["languages"][0]["conflicts"][0]["code"], "HUMAN-EDIT");
            assert_eq!(
                fs::read(&fixture.target).ok(),
                if delete {
                    None
                } else {
                    Some(b"human content".to_vec())
                }
            );
            fs::write(&fixture.target, &canonical).unwrap();
        }
        fixture.preview(0, 0);
        fixture.sync(0);
        assert_eq!(fixture.calls(), calls);
    }
}

#[test]
fn untracked_target_is_a_conflict_before_any_agent_dispatch() {
    for (json, source) in [
        (false, "Hello world.\n"),
        (false, "<!-- opaque -->\n"),
        (true, r#"{"message":"Hello world."}"#),
        (true, r#"{"empty":"","scalar":1}"#),
    ] {
        let fixture = fixture(source, json);
        fs::create_dir_all(fixture.target.parent().unwrap()).unwrap();
        fs::write(&fixture.target, b"existing human content").unwrap();
        fixture.preview(1, 1);
        fixture.sync(1);
        assert!(fixture.calls().is_empty());
        assert_eq!(
            fs::read(&fixture.target).unwrap(),
            b"existing human content"
        );
    }
}

#[test]
fn identical_untracked_pass_through_target_can_be_claimed_without_agent_calls() {
    for (json, source) in [
        (false, "<!-- opaque -->\n"),
        (true, r#"{"empty":"","scalar":1}"#),
    ] {
        let fixture = fixture(source, json);
        fs::create_dir_all(fixture.target.parent().unwrap()).unwrap();
        fs::write(&fixture.target, source).unwrap();
        fixture.preview(0, 0);
        fixture.sync(0);
        assert!(fixture.calls().is_empty());
    }
}
