use fani::test_support::db::Database;
use rusqlite::params;
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

    fn kill_at(&self, failpoint: &str) {
        use std::time::{Duration, Instant};
        let marker = self.temp.path().join("failpoint");
        let _ = fs::remove_file(&marker);
        let mut child = Command::new(assert_cmd::cargo::cargo_bin!("fani"))
            .args(["sync", "--config"])
            .arg(&self.config)
            .arg("--report-dir")
            .arg(self.temp.path().join("reports"))
            .arg("--quiet")
            .env("FANI_TEST_FAILPOINT", failpoint)
            .env("FANI_TEST_FAILPOINT_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !marker.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "exited before {failpoint}"
            );
            assert!(Instant::now() < deadline, "did not reach {failpoint}");
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
    }

    fn db(&self) -> Database {
        Database::open(self.repo.join(".fani/fani.db")).unwrap()
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.temp.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn explicit_source(&self, target: &str) {
        let config = fs::read_to_string(&self.config).unwrap()
            .replace("include = [\"docs/**/*.md\"]", &format!("sources = [{{ format = 'markdown', include = ['docs/guide.md'], target_pattern = '{target}' }}]"))
            .replace("target_pattern = \"translations/{lang}/{relpath}\"\n", "");
        fs::write(&self.config, config).unwrap();
    }

    fn budget_one(&self) {
        fs::write(
            &self.config,
            fs::read_to_string(&self.config)
                .unwrap()
                .replace("max_tasks = 10", "max_tasks = 1"),
        )
        .unwrap();
    }
    fn status(&self, pending: usize, reused: usize) {
        let output = self.run("status");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            matches!(output.status.code(), Some(0 | 3)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains(&format!("pending={pending}"))
                && text.contains(&format!("reused={reused}")),
            "{text}"
        );
    }
    fn adopt(&self) {
        let output = self.run("adopt");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn json_fixture(source: &str) -> Fixture {
    let mut fixture = Fixture::new("");
    fs::remove_file(fixture.repo.join("docs/guide.md")).unwrap();
    fs::write(fixture.repo.join("docs/guide.json"), source).unwrap();
    fixture.commit();
    let config = fs::read_to_string(&fixture.config).unwrap()
        .replace("include = [\"docs/**/*.md\"]", "sources = [{ format = 'json', message_syntax = 'i18next-interpolation-v1', include = ['docs/*.json'], strip_prefix = 'docs/', target_pattern = 'translations/{lang}/{relpath}' }]")
        .replace("target_pattern = \"translations/{lang}/{relpath}\"\n", "");
    fs::write(&fixture.config, config).unwrap();
    fixture.target = fixture.repo.join("translations/zh-CN/guide.json");
    let provider = fixture.temp.path().join("provider.sh");
    fs::write(&provider, format!("#!/bin/sh\nset -eu\njq -c . | tee -a '{}' | jq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(.task.source | gsub(\"Hello\";\"Bonjour\"))}}'\n", fixture.temp.path().join("calls").display())).unwrap();
    fixture
}

#[test]
fn json_current_candidate_history_is_stable_and_prior_output_is_context_only() {
    for corrupt in [false, true] {
        let fixture = json_fixture(r#"{"message":"Hello {{name}}"}"#);
        fixture.sync(0);
        let conn = fixture.db().connect().unwrap();
        let counts = || -> Vec<i64> {
            [
                "unit_versions",
                "translation_versions",
                "translation_memory_entries",
                "attempts",
                "canonical_content_versions",
            ]
            .iter()
            .map(|table| {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
            })
            .collect()
        };
        assert_eq!(counts(), vec![1; 5]);
        for _ in 0..3 {
            fixture.sync(0);
            assert_eq!(counts(), vec![1; 5]);
        }
        assert_eq!(fixture.calls(), 1);
        if corrupt {
            conn.execute("UPDATE attempts SET response_json=json_set(response_json,'$.output','invalid candidate')", []).unwrap();
        }
        fs::write(
            fixture.repo.join("docs/guide.json"),
            r#"{"message":"Entirely new {{name}}"}"#,
        )
        .unwrap();
        fixture.commit();
        fixture.status(1, 0);
        fixture.sync(0);
        assert_eq!(fixture.calls(), 2);
        let calls = fs::read_to_string(fixture.temp.path().join("calls")).unwrap();
        let request: Value = serde_json::from_str(calls.lines().last().unwrap()).unwrap();
        assert_eq!(request["task"]["previous_source"], "Hello {{name}}");
        if corrupt {
            assert!(request["task"]["previous_translation"].is_null());
            assert_eq!(request["prompt"]["resource"], "translate");
        } else {
            assert_eq!(
                request["task"]["previous_translation"],
                "Bonjour @@FANI_JSON_0@@"
            );
            assert_eq!(request["prompt"]["resource"], "revise");
        }
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM translation_memory_entries WHERE tier='trusted'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT source_text FROM unit_versions ORDER BY id LIMIT 1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "Hello {{name}}"
        );
    }
}

#[test]
fn json_zero_unit_merge_reconciles_without_translation_history() {
    use fani::application::ports::{PullRequestStateInput, StateStore};
    for scenario in [
        "observed",
        "published",
        "markdown",
        "blocked",
        "malformed",
        "missing_links",
        "invalid_links",
        "empty_manifest",
    ] {
        let fixture = if scenario == "markdown" {
            Fixture::new("<!-- pass-through -->\n")
        } else if matches!(scenario, "missing_links" | "invalid_links") {
            json_fixture(r#"{"message":"Hello"}"#)
        } else {
            json_fixture(r#"{"empty":"","scalars":[1,true,null]}"#)
        };
        let config = fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("enabled = false", "enabled = true");
        fs::write(&fixture.config, &config).unwrap();
        fixture.sync(0);
        let db = fixture.db();
        let conn = db.connect().unwrap();
        let (manifest, repository, commit): (i64, i64, String) = conn
            .query_row(
                "SELECT id,repository_id,candidate_commit FROM publication_manifests LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let contents: Vec<i64> = db
            .publication_snapshot(repository, "zh-CN", &commit)
            .unwrap()
            .iter()
            .map(|file| file.content_version_id)
            .collect();
        if scenario == "empty_manifest" {
            conn.execute(
                "DELETE FROM publication_manifest_files WHERE manifest_id=?1",
                [manifest],
            )
            .unwrap();
            assert!(
                StateStore::promote_merged_publication(
                    &db, repository, "zh-CN", &commit, "verified", &contents
                )
                .is_err()
            );
            continue;
        }
        if scenario == "invalid_links" {
            conn.execute(
                "UPDATE translation_versions SET validation_state='failed'",
                [],
            )
            .unwrap();
            assert!(
                StateStore::promote_merged_publication(
                    &db, repository, "zh-CN", &commit, "verified", &contents
                )
                .is_err()
            );
            continue;
        }
        if scenario == "missing_links" {
            conn.execute("DELETE FROM canonical_file_translations", [])
                .unwrap();
        }
        assert!(
            db.promote_merged_publication(repository, "zh-CN", &commit, "unverified_empty")
                .is_err()
        );
        let bin = fixture.temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let response = fixture.temp.path().join("pull.json");
        fs::write(&response, serde_json::json!({"number":1,"url":"https://example.invalid/pull/1","state":"MERGED","headRefName":"i18n/zh-CN","baseRefName":"main","isDraft":false,"title":"fixture","body":"fixture","headRefOid":commit}).to_string()).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, format!("#!/bin/sh\nset -eu\ncase \"$1 $2\" in\n'pr list') printf '[]\\n';;\n'pr create') printf 'https://example.invalid/pull/1\\n';;\n'pr view') cat '{}';;\n'pr edit') :;;\n*) exit 1;;\nesac\n", response.display())).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = config.replace("[agents.fixture]", "[repo.publish.github]\nenabled = true\nrepository = \"fixture/project\"\nbase = \"main\"\n[agents.fixture]");
        config = config.replace(
            "source_ref = \"HEAD\"",
            "source_ref = \"HEAD\"\npush = true",
        );
        if scenario == "published" {
            let remote = fixture.temp.path().join("remote.git");
            fixture.git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
            fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
            conn.execute("UPDATE publication_outbox SET state='pending',owner=NULL,lease_expires_at=NULL,completed_at=NULL,available_at=0", []).unwrap();
        } else {
            db.record_pr_state(PullRequestStateInput {
                repository_id: repository,
                provider: "github",
                external_id: "1",
                number: Some(1),
                branch: "i18n/zh-CN",
                url: Some("https://example.invalid/pull/1"),
                state: "open",
                head_revision: Some(&commit),
                event_key: "fixture",
                payload_json: "{}",
            })
            .unwrap();
        }
        if scenario == "blocked" {
            config = config.replace(
                "[repo.quality]",
                "[repo.documentation]\ncommands = [[\"sh\",\"-c\",\"exit 1\"]]\n[repo.quality]",
            );
        }
        if scenario == "malformed" {
            fs::write(fixture.repo.join("docs/guide.json"), "{malformed}").unwrap();
            fixture.commit();
        }
        fs::write(&fixture.config, config).unwrap();
        fixture.sync(match scenario {
            "missing_links" => 2,
            "blocked" | "malformed" => 1,
            _ => 0,
        });
        let state: String = conn
            .query_row(
                "SELECT state FROM publication_manifests WHERE id=?1",
                [manifest],
                |row| row.get(0),
            )
            .unwrap();
        if matches!(scenario, "observed" | "published" | "markdown") {
            assert_eq!(state, "merged", "{scenario}");
            fixture.sync(0);
            for table in [
                "units",
                "unit_versions",
                "translation_versions",
                "translation_memory_entries",
                "attempts",
                "canonical_file_translations",
            ] {
                assert_eq!(
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                        .unwrap(),
                    0,
                    "{scenario}: {table}"
                );
            }
            assert_eq!(
                conn.query_row(
                    "SELECT publication_state FROM canonical_content_versions LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                "merged"
            );
            assert_eq!(fixture.calls(), 0);
        } else {
            assert_ne!(state, "merged", "{scenario}");
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM translation_memory_entries WHERE tier='trusted'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
        }
    }
}

#[test]
fn json_lifecycle_pointer_reuse_changed_source_and_human_adoption() {
    let fixture = json_fixture(r#"{"title":"Hello","arg":"Hello {{name}}"}"#);
    fixture.status(2, 0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    let requests: Vec<Value> = fs::read_to_string(fixture.temp.path().join("calls"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for request in requests {
        assert_eq!(request["schema"], "fani.agent.request.v2");
        assert_eq!(request["task"]["source_format"], "json");
        assert_eq!(
            request["task"]["message_syntax"],
            "i18next-interpolation-v1"
        );
        assert!(
            !request["task"]["source"]
                .as_str()
                .unwrap()
                .contains("title")
        );
        assert_eq!(
            request["task"]["protected_tokens"],
            request["task"]["token_permissions"]["reorderable_tokens"]
        );
    }
    fixture.sync(0);
    fixture.status(0, 2);
    assert_eq!(fixture.calls(), 2);
    fs::write(
        &fixture.target,
        r#"{"arg":"Salut {{name}}","title":"Salut"}"#,
    )
    .unwrap();
    fixture.adopt();
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    fs::write(
        fixture.repo.join("docs/guide.json"),
        r#"{"title":"Entirely different message","arg":"Hello {{name}}"}"#,
    )
    .unwrap();
    fixture.commit();
    fixture.status(1, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 3);
    let calls = fs::read_to_string(fixture.temp.path().join("calls")).unwrap();
    let latest: Value = serde_json::from_str(calls.lines().last().unwrap()).unwrap();
    assert_eq!(latest["task"]["previous_source"], "Hello");
    assert_eq!(latest["task"]["previous_translation"], "Salut");
    fs::write(
        fixture.repo.join("docs/other.json"),
        r#"{"arg":"Hello {{name}}"}"#,
    )
    .unwrap();
    fixture.commit();
    fixture.status(1, 2);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 4);
    fs::write(
        &fixture.target,
        r#"{"arg":"Salut {{wrong}}","title":"Changed"}"#,
    )
    .unwrap();
    assert!(!fixture.run("adopt").status.success());
}

#[test]
fn json_zero_units_unsupported_sources_and_filename_mapping_have_no_hidden_effects() {
    let source = "{\r\n\"empty\":\"\",\"space\":\"\\u0020\\t\",\"data\":[1,true,null]}";
    let fixture = json_fixture(source);
    let config = fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("include = ['docs/*.json']", "include = ['docs/guide.json']")
        .replace("translations/{lang}/{relpath}", "translations/{lang}.json");
    fs::write(&fixture.config, config).unwrap();
    fixture.sync(0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 0);
    assert_eq!(
        fs::read(fixture.repo.join("translations/zh-CN.json")).unwrap(),
        source.as_bytes()
    );
    let db = fixture.db().connect().unwrap();
    for table in [
        "units",
        "unit_versions",
        "translation_versions",
        "attempts",
        "canonical_file_translations",
    ] {
        let count: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    drop(db);
    for (source, code) in [
        (
            r#"{"secret_one":"SENSITIVE CONTENT"}"#,
            "MESSAGE-UNSUPPORTED",
        ),
        (
            r#"{"a":"SENSITIVE CONTENT","a":"duplicate"}"#,
            "DOCUMENT-PARSE",
        ),
    ] {
        fs::write(fixture.repo.join("docs/guide.json"), source).unwrap();
        fixture.commit();
        fixture.sync(1);
        assert_eq!(fixture.calls(), 0);
        let report = fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap();
        assert!(report.contains(code));
        assert!(!report.contains("SENSITIVE CONTENT"));
    }
}

#[test]
fn json_recovery_and_project_check_failure_preserve_verified_effect_boundary() {
    let fixture = json_fixture(r#"{"value":"Hello {{name}}"}"#);
    fixture.kill_at("materialization_before_file_write");
    assert_eq!(fixture.calls(), 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 1);
    let original = fs::read(&fixture.target).unwrap();
    fs::write(
        fixture.repo.join("docs/guide.json"),
        r#"{"value":"Hello changed {{name}}"}"#,
    )
    .unwrap();
    fixture.commit();
    let config = fs::read_to_string(&fixture.config).unwrap().replace(
        "[repo.quality]",
        "[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"exit 1\"]]\n[repo.quality]",
    );
    fs::write(&fixture.config, config).unwrap();
    fixture.sync(1);
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM canonical_content_versions",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM publication_outbox", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn json_dialect_change_invalidates_memory_without_invalidating_markdown() {
    let fixture = json_fixture(r#"{"title":"Hello"}"#);
    fs::write(fixture.repo.join("docs/readme.md"), "Hello Markdown.\n").unwrap();
    fixture.commit();
    let config = fs::read_to_string(&fixture.config).unwrap().replace("sources = [{", "sources = [{ format = 'markdown', include = ['docs/*.md'], target_pattern = 'translations/{lang}/{relpath}' }, {");
    fs::write(&fixture.config, &config).unwrap();
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    fs::write(
        &fixture.config,
        config.replace("i18next-interpolation-v1", "plain"),
    )
    .unwrap();
    fixture.status(1, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 3);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 3);
}

fn seed_schema_two_canonical(fixture: &Fixture, source: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    let revision = String::from_utf8(revision.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let state = fixture.repo.join(".fani");
    fs::create_dir_all(&state).unwrap();
    let conn = rusqlite::Connection::open(state.join("fani.db")).unwrap();
    conn.execute_batch(include_str!("../migrations/0001_native_authority.sql"))
        .unwrap();
    conn.execute_batch(include_str!(
        "../migrations/0002_orthogonal_translation_state.sql"
    ))
    .unwrap();
    conn.execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY CHECK(version>0),name TEXT NOT NULL UNIQUE,checksum TEXT NOT NULL CHECK(length(checksum)=64),applied_at INTEGER NOT NULL) STRICT;").unwrap();
    for (version, name, sql) in [
        (
            1,
            "0001_native_authority",
            include_str!("../migrations/0001_native_authority.sql"),
        ),
        (
            2,
            "0002_orthogonal_translation_state",
            include_str!("../migrations/0002_orthogonal_translation_state.sql"),
        ),
    ] {
        conn.execute(
            "INSERT INTO schema_migrations VALUES (?1,?2,?3,1)",
            params![
                version,
                name,
                format!("{:x}", Sha256::digest(sql.as_bytes()))
            ],
        )
        .unwrap();
    }
    conn.pragma_update(None, "application_id", 0x4641_4e49_i64)
        .unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    let root = fixture
        .repo
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut hash = Sha256::new();
    hash.update((root.len() as u64).to_be_bytes());
    hash.update(root.as_bytes());
    let repository_key = format!("{:x}", hash.finalize());
    let source_hash = format!("{:x}", Sha256::digest(source.as_bytes()));
    conn.execute("INSERT INTO repositories(id,repository_key,root_path,created_at,updated_at) VALUES (1,?1,?2,1,1)",params![repository_key,root]).unwrap();
    conn.execute("INSERT INTO documents(id,repository_id,path,source_revision,content_hash,created_at,updated_at) VALUES (1,1,'docs/guide.md',?1,?2,1,1)",params![revision,source_hash]).unwrap();
    conn.execute("INSERT INTO canonical_files(id,repository_id,locale,path,source_revision,content,content_hash,state,validation_state,updated_at) VALUES (71,1,'zh-CN','translations/zh-CN/docs/guide.md',?1,?2,?3,'candidate','passed',1)",params![revision,source.as_bytes(),source_hash]).unwrap();
    conn.execute("INSERT INTO canonical_content_versions(id,canonical_file_id,source_revision,content,content_hash,created_at) VALUES (81,71,?1,?2,?3,1)",params![revision,source.as_bytes(),source_hash]).unwrap();
    conn.execute(
        "UPDATE canonical_files SET current_content_version_id=81 WHERE id=71",
        [],
    )
    .unwrap();
    drop(conn);
    state
}

#[test]
fn schema_two_passed_zero_unit_canonical_requires_current_checks_before_effects() {
    let source = "<!-- legacy pass-through -->\n";
    let fixture = Fixture::new(source);
    let state = seed_schema_two_canonical(&fixture, source);
    let config = fs::read_to_string(&fixture.config).unwrap();
    fs::write(
        &fixture.config,
        config.replace(
            "[repo.quality]",
            "[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"exit 1\"]]\n[repo.quality]",
        ),
    )
    .unwrap();
    fixture.sync(1);
    assert!(!fixture.target.exists());
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM materialization_outbox", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT provenance FROM canonical_files WHERE id=71",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "ai"
    );
    drop(conn);
    fs::write(&fixture.config, config).unwrap();
    fixture.sync(0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 0);
    assert_eq!(fs::read(&fixture.target).unwrap(), source.as_bytes());
    assert!(state.join("fani.db.pre-schema-2.bak").is_file());
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT content FROM canonical_content_versions WHERE id=81",
            [],
            |row| row.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        source.as_bytes()
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM translation_versions", [], |row| row
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
}

#[test]
fn schema_two_unbound_pending_intents_retain_original_cancelled_ids() {
    use sha2::{Digest, Sha256};
    let source = "Hello world.\n";
    let fixture = Fixture::new(source);
    let state = seed_schema_two_canonical(&fixture, source);
    let conn = rusqlite::Connection::open(state.join("fani.db")).unwrap();
    let revision: String = conn
        .query_row(
            "SELECT source_revision FROM documents WHERE id=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let unit = fani::domain::document::parse_document(
        fani::domain::document::DocumentFormat::Markdown,
        source.as_bytes(),
    )
    .unwrap()
    .units
    .remove(0);
    let metadata = fani::domain::document::unit_metadata(&unit, "docs/guide.md");
    let source_hash = format!("{:x}", Sha256::digest(unit.source.as_bytes()));
    let policy = fani::domain::prompts::policy_fingerprint();
    conn.execute("INSERT INTO units(id,document_id,unit_key,ordinal,source_text,source_hash,context_json,created_at,updated_at) VALUES (401,1,'legacy-unit',0,?1,?2,?3,1,1)",params![unit.source,source_hash,metadata]).unwrap();
    conn.execute("INSERT INTO unit_versions(id,unit_id,source_revision,source_text,source_hash,context_json,created_at) VALUES (402,401,?1,?2,?3,?4,1)",params![revision,unit.source,source_hash,metadata]).unwrap();
    conn.execute("INSERT INTO runs(id,repository_id,invocation_key,config_path,started_at,heartbeat_at,policy_fingerprint) VALUES ('old',1,'old','old-config',1,1,?1)",[&policy]).unwrap();
    conn.execute("INSERT INTO work_items(id,run_id,unit_id,locale,kind,input_json,created_at,updated_at,policy_fingerprint) VALUES (501,'old',401,'zh-CN','materialize',?1,1,1,?2)",params![serde_json::json!({"source_revision":revision,"path":"docs/guide.md"}).to_string(),policy]).unwrap();
    conn.execute("INSERT INTO materialization_outbox(id,work_item_id,dedupe_key,payload_json,available_at,created_at) VALUES (601,501,'legacy-materialization','{\"repository_id\":1,\"locale\":\"zh-CN\",\"path\":\"translations/zh-CN/docs/guide.md\"}',1,1)",[]).unwrap();
    let payload = serde_json::json!({"files":[{"canonical_content_version_id":81,"canonical_file_id":71,"content":source,"content_hash":format!("{:x}",Sha256::digest(source.as_bytes())),"path":"translations/zh-CN/docs/guide.md"}],"language":"zh-CN","policy_fingerprint":policy,"run_id":"old","source_revision":revision});
    conn.execute("INSERT INTO publication_outbox(id,repository_id,run_id,locale,dedupe_key,payload_json,available_at,created_at) VALUES (701,1,'old','zh-CN','legacy-publication',?1,1,1)",[payload.to_string()]).unwrap();
    drop(conn);
    fs::write(
        &fixture.config,
        fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("enabled = false", "enabled = true"),
    )
    .unwrap();
    fixture.sync(0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 1);
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row("SELECT status FROM work_items WHERE id=501", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "cancelled"
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM materialization_outbox WHERE id=601 AND work_item_id=501",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "done"
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM publication_outbox WHERE id=701",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "done"
    );
    assert!(conn.query_row("SELECT json_extract(payload_json,'$.superseded_reason') FROM publication_outbox WHERE id=701",[],|row| row.get::<_,Option<String>>(0)).unwrap().is_some());
    assert_eq!(
        fs::read_to_string(&fixture.target).unwrap(),
        "Bonjour world.\n"
    );
}

#[test]
fn zero_unit_documents_are_verified_without_translation_provenance() {
    let source =
        "<!-- private-source-sentinel -->\r\n\r\n```rust\r\nlet private_value = 7;\r\n```\r\n";
    let fixture = Fixture::new(source);
    fixture.sync(0);
    assert_eq!(fs::read(&fixture.target).unwrap(), source.as_bytes());
    fixture.sync(0);
    fixture.adopt();
    fs::write(&fixture.target, "human edit").unwrap();
    let output = fixture.run("discard");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&fixture.target).unwrap(), source.as_bytes());
    fixture.sync(0);
    assert_eq!(fixture.calls(), 0);
    let conn = fixture.db().connect().unwrap();
    for table in [
        "units",
        "unit_versions",
        "attempts",
        "translation_versions",
        "translation_memory_entries",
        "canonical_file_translations",
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    let provenance: String = conn
        .query_row("SELECT provenance FROM canonical_files", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(provenance, "imported");
    let text = fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap();
    assert!(!text.contains("private-source-sentinel"));
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["schema"], 4);
    assert_eq!(report["totals"]["files_written"], 0);
    assert_eq!(report["totals"]["markdown_files"], 1);
    assert_eq!(report["totals"]["verified_documents"], 1);
    assert_eq!(report["totals"]["pass_through_documents"], 1);
}

#[test]
fn blocking_project_checks_never_persist_or_materialize_candidates() {
    for timed_out in [false, true] {
        let fixture = Fixture::new("Hello world.\n");
        fixture.sync(0);
        let old_target = fs::read(&fixture.target).unwrap();
        let conn = fixture.db().connect().unwrap();
        let old_versions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM canonical_content_versions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        fs::write(fixture.repo.join("docs/guide.md"), "Hello changed world.\n").unwrap();
        fixture.commit();
        let command = if timed_out { "sleep 1" } else { "exit 1" };
        let checks = format!(
            "[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"{command}\"]]\ntimeout_s = 0.05\n[repo.quality]"
        );
        let config = fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("[repo.quality]", &checks);
        fs::write(&fixture.config, config).unwrap();
        fixture.sync(1);
        assert_eq!(fs::read(&fixture.target).unwrap(), old_target);
        let conn = fixture.db().connect().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM canonical_content_versions",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            old_versions
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM publication_outbox", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let code = if timed_out {
            "DOC-CHECK-TIMEOUT"
        } else {
            "DOC-CHECK-FAILED"
        };
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM findings WHERE code=?1",
                [code],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn compatible_pending_document_intent_completes_original_work_after_new_revision() {
    let fixture = Fixture::new("<!-- pass-through -->\n");
    fixture.sync(0);
    let conn = fixture.db().connect().unwrap();
    let (outbox, work): (i64, i64) = conn
        .query_row(
            "SELECT id,work_item_id FROM materialization_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    conn.execute(
        "UPDATE materialization_outbox SET state='pending',completed_at=NULL",
        [],
    )
    .unwrap();
    conn.execute("UPDATE work_items SET status='pending' WHERE id=?1", [work])
        .unwrap();
    drop(conn);
    fs::write(fixture.repo.join("docs/note.txt"), "unrelated revision").unwrap();
    fixture.commit();
    fixture.sync(0);
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM work_items WHERE kind='materialization'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM materialization_outbox WHERE id=?1 AND work_item_id=?2",
            params![outbox, work],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "done"
    );
    assert_eq!(
        conn.query_row("SELECT status FROM work_items WHERE id=?1", [work], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "succeeded"
    );
    assert_eq!(fixture.calls(), 0);
}

#[test]
fn terminal_materialization_content_roundtrip_writes_current_bytes() {
    let fixture = Fixture::new("<!-- A -->\n");
    fixture.sync(0);
    for source in ["<!-- B -->\n", "<!-- A -->\n"] {
        fs::write(fixture.repo.join("docs/guide.md"), source).unwrap();
        fixture.commit();
        fixture.sync(0);
        assert_eq!(fs::read_to_string(&fixture.target).unwrap(), source);
    }
    fixture.sync(0);
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM materialization_outbox WHERE state='done'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM work_items WHERE kind='materialization' AND status='succeeded'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    assert_eq!(fixture.calls(), 0);
}

#[test]
fn cancelled_mapping_roundtrip_retains_history_and_writes_returned_target() {
    let fixture = Fixture::new("<!-- A -->\n");
    let original_config = fs::read_to_string(&fixture.config).unwrap();
    fixture.kill_at("materialization_before_file_write");
    let conn = fixture.db().connect().unwrap();
    let original: (i64, i64, String) = conn
        .query_row(
            "SELECT id,work_item_id,payload_json FROM materialization_outbox",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    fs::write(
        &fixture.config,
        original_config.replace("translations/{lang}/{relpath}", "other/{lang}/{relpath}"),
    )
    .unwrap();
    fixture.sync(0);
    assert!(!fixture.target.exists());
    fs::write(&fixture.config, original_config).unwrap();
    fixture.sync(0);
    assert_eq!(fs::read_to_string(&fixture.target).unwrap(), "<!-- A -->\n");
    fixture.sync(0);
    let retained: (String, String, String) = conn.query_row("SELECT o.state,w.status,o.payload_json FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id WHERE o.id=?1 AND w.id=?2", params![original.0, original.1], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(retained, ("done".into(), "cancelled".into(), original.2));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM materialization_outbox", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(fixture.calls(), 0);
}

#[test]
fn publication_recovery_reports_new_parse_failure_on_first_rerun() {
    let fixture = Fixture::new("<!-- private opaque source -->\n");
    fs::write(
        &fixture.config,
        fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("enabled = false", "enabled = true\npush = false"),
    )
    .unwrap();
    fixture.kill_at("publication_candidate_persisted");
    let conn = fixture.db().connect().unwrap();
    let original: (i64, String) = conn
        .query_row("SELECT id,payload_json FROM publication_outbox", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    fs::write(
        fixture.repo.join("docs/bad.md"),
        b"\xff\nprivate-invalid-source",
    )
    .unwrap();
    fixture.commit();
    fixture.sync(1);
    let report_text = fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap();
    let report: Value = serde_json::from_str(&report_text).unwrap();
    assert_eq!(report["status"], "needs_human");
    assert_eq!(report["languages"][0]["status"], "needs_human");
    assert_eq!(report["languages"][0]["parse_failures"], 1);
    assert_eq!(
        report["languages"][0]["conflicts"][0]["code"],
        "DOCUMENT-PARSE"
    );
    assert_eq!(
        report["languages"][0]["conflicts"][0]["message"],
        "source document could not be parsed under the configured contract"
    );
    assert!(report_text.contains("docs/bad.md"));
    assert!(!report_text.contains("private-invalid-source"));
    assert!(!report_text.contains("private opaque source"));
    assert_eq!(
        conn.query_row(
            "SELECT status FROM runs ORDER BY started_at DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "needs_human"
    );
    let recovered: (String, String) = conn
        .query_row(
            "SELECT state,payload_json FROM publication_outbox WHERE id=?1",
            [original.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(recovered, ("done".into(), original.1));
    fixture.sync(1);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM publication_outbox", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(fixture.calls(), 0);
}

#[test]
fn intent_binding_roundtrip_selects_current_identity_without_mutating_history() {
    use fani::application::ports::StateStore;
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    let db = fixture.db();
    let conn = db.connect().unwrap();
    let (content_id, identity_a): (i64, String) = conn
        .query_row(
            "SELECT canonical_content_version_id,identity_json FROM canonical_document_intents",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let mut identity_b: Value = serde_json::from_str(&identity_a).unwrap();
    identity_b["source_set_id"] = "different".into();
    let identity_b = identity_b.to_string();
    let snapshot = || {
        conn.query_row("SELECT json_group_array(json_array(id,canonical_content_version_id,identity_json,created_at)) FROM canonical_document_intents", [], |r| r.get::<_, String>(0)).unwrap()
    };
    db.bind_canonical_document_intent(content_id, &identity_b)
        .unwrap();
    assert_eq!(
        db.canonical_document_intent(content_id).unwrap(),
        Some(identity_b)
    );
    let history = snapshot();
    for _ in 0..2 {
        db.bind_canonical_document_intent(content_id, &identity_a)
            .unwrap();
        assert_eq!(
            db.canonical_document_intent(content_id).unwrap(),
            Some(identity_a.clone())
        );
        assert_eq!(snapshot(), history);
    }
    let mut incompatible: Value = serde_json::from_str(&identity_a).unwrap();
    incompatible["source_revision"] = "wrong-revision".into();
    assert!(
        db.bind_canonical_document_intent(content_id, &incompatible.to_string())
            .is_err()
    );
    assert_eq!(
        db.canonical_document_intent(content_id).unwrap(),
        Some(identity_a)
    );
    assert_eq!(snapshot(), history);
}

#[test]
fn renewed_publication_retains_rejection_and_exposes_promotable_snapshot() {
    let fixture = Fixture::new("Hello world.\n");
    let allow = fixture.temp.path().join("allow");
    fs::write(&allow, "allowed").unwrap();
    let config = fs::read_to_string(&fixture.config).unwrap()
        .replace("enabled = false", "enabled = true\npush = false")
        .replace("[repo.quality]", &format!("[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"test -f '{}'\"]]\n[repo.quality]", allow.display()));
    fs::write(&fixture.config, config).unwrap();
    fixture.kill_at("publication_candidate_persisted");
    let db = fixture.db();
    let conn = db.connect().unwrap();
    let (repo_id, commit): (i64, String) = conn
        .query_row(
            "SELECT repository_id,candidate_commit FROM publication_manifests",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    fs::remove_file(&allow).unwrap();
    fixture.sync(1);
    assert!(
        db.publication_snapshot(repo_id, "zh-CN", &commit)
            .unwrap()
            .is_empty()
    );
    let old_manifest: String = conn.query_row("SELECT json_array(id,run_id,source_revision,candidate_commit,policy_fingerprint,state,created_at,merged_at,authorization_key) FROM publication_manifests", [], |r| r.get(0)).unwrap();
    let old_outbox: String = conn.query_row("SELECT json_array(id,dedupe_key,payload_json,state,completed_at) FROM publication_outbox", [], |r| r.get(0)).unwrap();
    fs::write(&allow, "allowed").unwrap();
    fixture.sync(0);
    fixture.sync(0);
    let snapshot = db.publication_snapshot(repo_id, "zh-CN", &commit).unwrap();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].content, fs::read(&fixture.target).unwrap());
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM publication_manifests WHERE state='commit_created' AND candidate_commit=?1", [&commit], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(
        db.promote_merged_publication(repo_id, "zh-CN", &commit, "test_verified_merge")
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM publication_manifests WHERE state='merged'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(conn.query_row("SELECT json_array(id,run_id,source_revision,candidate_commit,policy_fingerprint,state,created_at,merged_at,authorization_key) FROM publication_manifests WHERE id=1", [], |r| r.get::<_, String>(0)).unwrap(), old_manifest);
    assert_eq!(conn.query_row("SELECT json_array(id,dedupe_key,payload_json,state,completed_at) FROM publication_outbox WHERE id=1", [], |r| r.get::<_, String>(0)).unwrap(), old_outbox);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM publication_outbox", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn published_configuration_roundtrip_keeps_independent_commit_authorizations() {
    for cancel_b in [false, true] {
        let fixture = Fixture::new("Hello world.\n");
        let config_a = fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("enabled = false", "enabled = true\npush = false");
        fs::write(&fixture.config, &config_a).unwrap();
        fixture.sync(0);
        let db = fixture.db();
        let conn = db.connect().unwrap();
        let (repo_id, commit): (i64, String) = conn
            .query_row(
                "SELECT repository_id,candidate_commit FROM publication_manifests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let original: String = conn.query_row("SELECT json_array(id,run_id,source_revision,candidate_commit,policy_fingerprint,state,created_at,merged_at,authorization_key) FROM publication_manifests", [], |r| r.get(0)).unwrap();
        let links: String = conn.query_row("SELECT json_group_array(json_array(canonical_content_version_id,translation_version_id)) FROM canonical_file_translations", [], |r| r.get(0)).unwrap();
        fs::write(
            &fixture.config,
            config_a.replace("docs/**/*.md", "docs/guide.md"),
        )
        .unwrap();
        if cancel_b {
            fixture.kill_at("publication_candidate_persisted");
        } else {
            fixture.sync(0);
            fixture.sync(0);
        }
        fs::write(&fixture.config, config_a).unwrap();
        fixture.sync(0);
        fixture.sync(0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(DISTINCT run_id) FROM publication_manifests",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(DISTINCT candidate_commit) FROM publication_manifests",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT state FROM publication_manifests WHERE id=2",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            if cancel_b {
                "superseded"
            } else {
                "commit_created"
            }
        );
        assert_eq!(conn.query_row("SELECT json_array(id,run_id,source_revision,candidate_commit,policy_fingerprint,state,created_at,merged_at,authorization_key) FROM publication_manifests WHERE id=1", [], |r| r.get::<_, String>(0)).unwrap(), original);
        assert_eq!(conn.query_row("SELECT json_group_array(json_array(canonical_content_version_id,translation_version_id)) FROM canonical_file_translations", [], |r| r.get::<_, String>(0)).unwrap(), links);
        assert_eq!(
            conn.query_row("SELECT publication_state FROM canonical_files", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "commit_created"
        );
        assert_eq!(
            conn.query_row(
                "SELECT publication_state FROM translation_versions LIMIT 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "commit_created"
        );
        let snapshot = db.publication_snapshot(repo_id, "zh-CN", &commit).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].content, fs::read(&fixture.target).unwrap());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM publication_outbox", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(fixture.calls(), 1);
    }
}

#[test]
fn same_revision_configuration_roundtrip_publishes_current_intent_idempotently() {
    let fixture = Fixture::new("Hello world.\n");
    let config_a = fs::read_to_string(&fixture.config).unwrap();
    fixture.sync(0);
    let conn = fixture.db().connect().unwrap();
    let snapshot = || {
        conn.query_row("SELECT json_group_array(json_array(v.id,v.source_revision,hex(v.content),l.translation_version_id)) FROM canonical_content_versions v LEFT JOIN canonical_file_translations l ON l.canonical_content_version_id=v.id", [], |r| r.get::<_, String>(0)).unwrap()
    };
    let immutable = snapshot();
    fs::write(
        &fixture.config,
        config_a.replace("docs/**/*.md", "docs/guide.md"),
    )
    .unwrap();
    fixture.sync(0);
    let history: String = conn.query_row("SELECT json_group_array(json_array(id,identity_json,created_at)) FROM canonical_document_intents", [], |r| r.get(0)).unwrap();
    fs::write(
        &fixture.config,
        config_a.replace("enabled = false", "enabled = true\npush = false"),
    )
    .unwrap();
    fixture.sync(0);
    fixture.sync(0);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM publication_manifests WHERE state='commit_created'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM publication_outbox", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(snapshot(), immutable);
    assert_eq!(conn.query_row("SELECT json_group_array(json_array(id,identity_json,created_at)) FROM canonical_document_intents", [], |r| r.get::<_, String>(0)).unwrap(), history);
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn changed_mapping_cancels_original_pending_ids_before_replanning() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    let original = fs::read(&fixture.target).unwrap();
    let conn = fixture.db().connect().unwrap();
    let (outbox, work): (i64, i64) = conn
        .query_row(
            "SELECT id,work_item_id FROM materialization_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    conn.execute(
        "UPDATE materialization_outbox SET state='pending',completed_at=NULL",
        [],
    )
    .unwrap();
    drop(conn);
    let config = fs::read_to_string(&fixture.config).unwrap().replace(
        "translations/{lang}/{relpath}",
        "new-target/{lang}/{relpath}",
    );
    fs::write(&fixture.config, config).unwrap();
    fixture.sync(0);
    let conn = fixture.db().connect().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT state FROM materialization_outbox WHERE id=?1",
            [outbox],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "done"
    );
    assert_eq!(
        conn.query_row("SELECT status FROM work_items WHERE id=?1", [work], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "cancelled"
    );
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    assert_eq!(
        fs::read(fixture.repo.join("new-target/zh-CN/docs/guide.md")).unwrap(),
        original
    );
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn project_check_is_anchored_to_first_complete_candidate_once() {
    let fixture = Fixture::new("# Hello heading\n\nHello paragraph.\n");
    fs::write(fixture.repo.join("docs/z-empty.md"), "<!-- opaque -->\n").unwrap();
    fixture.commit();
    fixture.budget_one();
    let checks = fixture.temp.path().join("checks");
    let config = fs::read_to_string(&fixture.config).unwrap().replace("[repo.quality]", &format!("[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"test -f translations/zh-CN/docs/z-empty.md && test ! -f translations/zh-CN/docs/guide.md && echo checked >> '{}'\"]]\n[repo.quality]", checks.display()));
    fs::write(&fixture.config, config).unwrap();
    fixture.sync(3);
    assert_eq!(fs::read_to_string(checks).unwrap().lines().count(), 1);
    assert!(!fixture.target.exists());
    let conn = fixture.db().connect().unwrap();
    let (path, input, result, status): (String, String, String, String) = conn.query_row("SELECT d.path,w.input_json,w.result_json,w.status FROM work_items w JOIN documents d ON d.id=w.document_id WHERE w.kind='project_check'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!(path, "docs/z-empty.md");
    assert_eq!(status, "succeeded");
    let input: Value = serde_json::from_str(&input).unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(input["manifest_hash"], result["manifest_hash"]);
    assert_eq!(input["manifest"].as_array().unwrap().len(), 1);
}

#[test]
fn unchanged_content_at_new_revision_retains_immutable_canonical_history() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    let conn = fixture.db().connect().unwrap();
    let old: (i64, String, Vec<u8>) = conn
        .query_row(
            "SELECT id,source_revision,content FROM canonical_content_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let links: Vec<i64> = conn.prepare("SELECT translation_version_id FROM canonical_file_translations WHERE canonical_content_version_id=?1 ORDER BY 1").unwrap().query_map([old.0], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    drop(conn);
    fs::write(
        fixture.repo.join("docs/second.md"),
        "Hello second document.\n",
    )
    .unwrap();
    fixture.commit();
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    let conn = fixture.db().connect().unwrap();
    let historical: (i64, String, Vec<u8>) = conn
        .query_row(
            "SELECT id,source_revision,content FROM canonical_content_versions WHERE id=?1",
            [old.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(historical, old);
    let after: Vec<i64> = conn.prepare("SELECT translation_version_id FROM canonical_file_translations WHERE canonical_content_version_id=?1 ORDER BY 1").unwrap().query_map([old.0], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(after, links);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM canonical_content_versions WHERE canonical_file_id=(SELECT id FROM canonical_files WHERE path LIKE '%guide.md')", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
}

#[test]
fn reverse_input_alias_is_rejected_before_cli_state_or_provider_effects() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("Hello world.\n");
    fixture.explicit_source("out/{lang}/guide.md");
    fs::remove_dir_all(fixture.repo.join("docs")).unwrap();
    symlink("out/zh-CN", fixture.repo.join("docs")).unwrap();
    for create_parent in [false, true] {
        if create_parent {
            fs::create_dir_all(fixture.repo.join("out/zh-CN")).unwrap();
        }
        for command in ["status", "check", "sync", "adopt", "discard"] {
            let output = fixture.run(command);
            assert_eq!(output.status.code(), Some(2), "{command}: {output:?}");
            let diagnostic = if command == "sync" {
                fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap()
            } else {
                String::from_utf8_lossy(&output.stderr).into_owned()
            };
            assert!(
                diagnostic.contains("input source path aliases"),
                "{command}: {diagnostic}"
            );
            assert_eq!(fixture.calls(), 0);
            assert!(!fixture.repo.join(".fani").exists());
            assert!(!fixture.repo.join("out/zh-CN/guide.md").exists());
            assert!(!fixture.repo.join("docs/guide.md").exists());
        }
    }
}

#[test]
fn invalid_mapping_preflight_precedes_database_open_for_every_entrypoint() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.explicit_source(".{lang}/guide.md");
    let config = fs::read_to_string(&fixture.config).unwrap().replace(
        "languages = [\"zh-CN\"]",
        "languages = [\"zh-CN\", \"fani\"]",
    );
    fs::write(&fixture.config, config).unwrap();
    for command in ["status", "check", "sync", "adopt", "discard"] {
        let output = fixture.run(command);
        assert_eq!(output.status.code(), Some(2), "{command}: {output:?}");
        assert!(
            !fixture.repo.join(".fani").exists(),
            "{command} created state"
        );
        assert_eq!(fixture.calls(), 0);
    }
    fs::create_dir(fixture.repo.join(".fani")).unwrap();
    let database = fixture.repo.join(".fani/fani.db");
    fs::write(&database, "must not be opened or migrated").unwrap();
    for command in ["status", "check", "sync", "adopt", "discard"] {
        let output = fixture.run(command);
        assert_eq!(output.status.code(), Some(2));
        let diagnostic = if command == "sync" {
            fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap()
        } else {
            String::from_utf8_lossy(&output.stderr).into_owned()
        };
        assert!(
            diagnostic.contains("reserved state/report/Git"),
            "{command}: {diagnostic}"
        );
        assert_eq!(
            fs::read_to_string(&database).unwrap(),
            "must not be opened or migrated"
        );
        assert_eq!(fs::read_dir(fixture.repo.join(".fani")).unwrap().count(), 1);
    }
}

#[test]
fn actual_report_reservation_precedes_database_creation() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.explicit_source("custom-reports/{lang}.md");
    let reports = fixture.repo.join("custom-reports");
    let output = Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .arg("sync")
        .arg("--config")
        .arg(&fixture.config)
        .arg("--report-dir")
        .arg(&reports)
        .arg("--quiet")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(!fixture.repo.join(".fani").exists());
    assert!(!reports.join("zh-CN.md").exists());
    assert_eq!(fixture.calls(), 0);
    assert!(
        fs::read_to_string(reports.join("report.json"))
            .unwrap()
            .contains("reserved state/report/Git")
    );
}

#[test]
fn invalid_mapping_does_not_reconcile_stored_pr_or_mutate_existing_database() {
    use fani::application::ports::PullRequestStateInput;
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    {
        let db = fixture.db();
        let repository_id = db
            .connect()
            .unwrap()
            .query_row("SELECT id FROM repositories", [], |row| row.get(0))
            .unwrap();
        db.record_pr_state(PullRequestStateInput {
            repository_id,
            provider: "github",
            external_id: "1",
            number: Some(1),
            branch: "i18n/zh-CN",
            url: Some("https://example.invalid/pull/1"),
            state: "open",
            head_revision: None,
            event_key: "fixture",
            payload_json: "{}",
        })
        .unwrap();
    }
    fixture.explicit_source(".fani/{lang}.md");
    let config = fs::read_to_string(&fixture.config).unwrap()
        .replace("enabled = false", "enabled = true")
        .replace("source_ref = \"HEAD\"", "source_ref = \"HEAD\"\npush = true")
        .replace("[agents.fixture]", "[repo.publish.github]\nenabled = true\nrepository = 'fixture/project'\n[agents.fixture]");
    fs::write(&fixture.config, config).unwrap();
    let bin = fixture.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let marker = fixture.temp.path().join("gh-called");
    let gh = bin.join("gh");
    fs::write(
        &gh,
        format!("#!/bin/sh\ntouch '{}'\nexit 66\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let database = fixture.repo.join(".fani/fani.db");
    let before = fs::read(&database).unwrap();
    let target = fs::read(&fixture.target).unwrap();
    for command in ["status", "check", "sync", "adopt", "discard"] {
        let output = fixture.run(command);
        assert_eq!(output.status.code(), Some(2), "{command}: {output:?}");
        assert_eq!(
            fs::read(&database).unwrap(),
            before,
            "{command} mutated state"
        );
        assert_eq!(fs::read(&fixture.target).unwrap(), target);
        assert_eq!(fixture.calls(), 1);
        assert!(!marker.exists(), "{command} invoked gh");
    }
    assert!(
        fs::read_to_string(fixture.temp.path().join("reports/report.json"))
            .unwrap()
            .contains("reserved state/report/Git")
    );
}

#[test]
fn validated_revision_remains_pinned_when_ref_moves_between_languages() {
    let fixture = Fixture::new("Hello world.\n");
    let revision = |fixture: &Fixture| {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let original = revision(&fixture);
    fs::write(fixture.repo.join("docs/unsupported.txt"), "not Markdown").unwrap();
    fixture.commit();
    let moved = revision(&fixture);
    fixture.git(&["reset", "--hard", &original]);
    let config = fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("languages = [\"zh-CN\"]", "languages = [\"zh-CN\", \"fr\"]")
        .replace("docs/**/*.md", "docs/**");
    fs::write(&fixture.config, config).unwrap();
    let provider = fixture.temp.path().join("provider.sh");
    let script = fs::read_to_string(&provider).unwrap().replace(
        "set -eu\n",
        &format!(
            "set -eu\ngit -C '{}' update-ref refs/heads/main '{}'\n",
            fixture.repo.display(),
            moved
        ),
    );
    fs::write(&provider, script).unwrap();
    fixture.sync(0);
    assert_eq!(revision(&fixture), moved);
    assert_eq!(fixture.calls(), 2);
    let report: Value = serde_json::from_str(
        &fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap(),
    )
    .unwrap();
    for language in report["languages"].as_array().unwrap() {
        assert_eq!(language["source_revision"], original);
    }
    let output = fixture.run("status");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported source extension"));
}

#[test]
fn changed_source_cannot_inherit_old_trust_after_an_exhausted_budget_rerun() {
    let fixture = Fixture::new("Hello one.\n\nHello old world.\n");
    fixture.sync(0);
    fixture.adopt();
    let original = fs::read(&fixture.target).unwrap();
    fixture.budget_one();
    fs::write(
        fixture.repo.join("docs/guide.md"),
        "Completely different first paragraph.\n\nHello new world.\n",
    )
    .unwrap();
    fixture.commit();
    fixture.sync(3);
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    assert_eq!(fixture.calls(), 3);
    fixture.status(1, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 4);
    assert!(
        fs::read_to_string(&fixture.target)
            .unwrap()
            .contains("Bonjour new world.")
    );
    fixture.sync(0);
    assert_eq!(fixture.calls(), 4);
}

#[test]
fn incompatible_stored_contract_cannot_return_through_trusted_memory_fallback() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    fixture.adopt();
    let db = fixture.db();
    let conn = db.connect().unwrap();
    conn.execute("UPDATE unit_versions SET context_json=json_set(context_json,'$.contract.verifier','unknown-verifier')", []).unwrap();
    conn.execute("UPDATE translation_memory_entries SET context_key=json_set(context_key,'$.contract.verifier','unknown-verifier')", []).unwrap();
    let requests = conn
        .prepare("SELECT id,request_json FROM attempts")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for (id, request) in requests {
        let mut value: Value = serde_json::from_str(&request).unwrap();
        let mut metadata: Value =
            serde_json::from_str(value["_fani_unit"]["context_json"].as_str().unwrap()).unwrap();
        metadata["contract"]["verifier"] = "unknown-verifier".into();
        value["_fani_unit"]["context_json"] = metadata.to_string().into();
        conn.execute(
            "UPDATE attempts SET request_json=?2 WHERE id=?1",
            params![id, value.to_string()],
        )
        .unwrap();
    }
    fixture.status(1, 0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    fixture.status(0, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
}

#[test]
fn malformed_unknown_and_changed_dialect_contracts_stay_pending() {
    for contract in [
        serde_json::json!("malformed"),
        serde_json::json!({"parser":"unknown"}),
        {
            let mut value = serde_json::to_value(
                fani::domain::document::format_contract(
                    fani::domain::document::DocumentFormat::Markdown,
                )
                .unwrap(),
            )
            .unwrap();
            value["message_syntax"] = "changed-dialect".into();
            value
        },
    ] {
        let fixture = Fixture::new("Hello world.\n");
        fixture.sync(0);
        fixture.adopt();
        let conn = fixture.db().connect().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT context_json FROM unit_versions LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut metadata: Value = serde_json::from_str(&raw).unwrap();
        metadata["contract"] = contract;
        conn.execute(
            "UPDATE unit_versions SET context_json=?1",
            [metadata.to_string()],
        )
        .unwrap();
        conn.execute(
            "UPDATE attempts SET request_json=json_remove(request_json,'$._fani_unit')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE translation_memory_entries SET context_key='Paragraph',provenance='legacy_fixture'", []).unwrap();
        fixture.status(1, 0);
        fixture.sync(0);
        assert_eq!(fixture.calls(), 2);
        fixture.status(0, 1);
        fixture.sync(0);
        assert_eq!(fixture.calls(), 2);
    }
}

#[test]
fn prompt_policy_changes_do_not_invalidate_current_contract_trusted_markdown() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    fixture.adopt();
    let conn = fixture.db().connect().unwrap();
    let old_policy = "a".repeat(64);
    conn.execute("UPDATE translation_versions SET policy_fingerprint=?1 WHERE id IN (SELECT translation_version_id FROM translation_memory_entries WHERE tier='trusted')", [&old_policy]).unwrap();
    conn.execute("UPDATE translation_memory_entries SET policy_fingerprint=?1,provenance=json_set(provenance,'$._fani_unit.policy_fingerprint',?1) WHERE tier='trusted'", [&old_policy]).unwrap();
    conn.execute("UPDATE canonical_candidates SET selected=0", [])
        .unwrap();
    conn.execute(
        "UPDATE translation_memory_entries SET superseded_at=1 WHERE tier='candidate'",
        [],
    )
    .unwrap();
    fixture.status(0, 1);
    fixture.sync(0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn equal_source_in_another_document_does_not_inherit_trust() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    fixture.adopt();
    fs::write(fixture.repo.join("docs/other.md"), "Hello world.\n").unwrap();
    fixture.commit();
    fs::write(
        &fixture.config,
        fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("docs/**/*.md", "docs/other.md"),
    )
    .unwrap();
    fixture.status(1, 0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
}

#[test]
fn invalid_successful_attempts_are_retired_and_replaced_within_the_budget() {
    let fixture = Fixture::new("Hello `first`.\n\nHello `second`.\n");
    fixture.sync(0);
    let original = fs::read(&fixture.target).unwrap();
    let db = fixture.db();
    let conn = db.connect().unwrap();
    conn.execute("UPDATE attempts SET response_json=json_set(response_json,'$.output','Broken.') WHERE status='succeeded'", []).unwrap();
    conn.execute("UPDATE canonical_candidates SET target_text='Broken.'", [])
        .unwrap();
    conn.execute("UPDATE translation_versions SET target_text='Broken.'", [])
        .unwrap();
    conn.execute(
        "UPDATE translation_memory_entries SET target_text='Broken.'",
        [],
    )
    .unwrap();
    fixture.budget_one();
    fixture.status(1, 0);
    fixture.sync(3);
    assert_eq!(fixture.calls(), 3);
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    fixture.status(1, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 4);
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 4);
    let retired: i64 = conn.query_row("SELECT COUNT(*) FROM attempts WHERE status='cancelled' AND dedupe_key LIKE '%:superseded:%'", [], |row| row.get(0)).unwrap();
    assert_eq!(retired, 2);
    let history: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM translation_versions WHERE validation_state='quarantined'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(history, 2);
}

#[test]
fn recognized_legacy_markdown_is_revalidated_and_upgraded_without_model_calls() {
    let fixture = Fixture::new("Read [guide][ref].\n\n[ref]: https://example.com\n");
    fixture.sync(0);
    fixture.adopt();
    let db = fixture.db();
    let conn = db.connect().unwrap();
    let stable: String = conn
        .query_row("SELECT unit_key FROM units", [], |row| row.get(0))
        .unwrap();
    conn.execute("UPDATE canonical_candidates SET selected=0", [])
        .unwrap();
    conn.execute(
        "UPDATE translation_memory_entries SET superseded_at=1 WHERE tier='candidate'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE unit_versions SET context_json='{\"kind\":\"Paragraph\"}'",
        [],
    )
    .unwrap();
    conn.execute("UPDATE translation_memory_entries SET translation_version_id=NULL,context_key='Paragraph' WHERE tier='trusted' AND superseded_at IS NULL", []).unwrap();
    fixture.status(0, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 1);
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM translation_memory_entries WHERE tier='trusted' AND superseded_at IS NULL", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 2);
    let new_key: String = conn.query_row("SELECT context_key FROM translation_memory_entries WHERE tier='trusted' ORDER BY id DESC LIMIT 1", [], |row| row.get(0)).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&new_key).unwrap()["document"],
        "docs/guide.md"
    );
    assert_eq!(
        conn.query_row::<String, _, _>("SELECT unit_key FROM units", [], |row| row.get(0))
            .unwrap(),
        stable
    );
}

#[test]
fn kind_only_legacy_reference_candidates_and_trust_upgrade_without_model_calls() {
    for trusted in [false, true] {
        let fixture = Fixture::new(
            "Read [guide][ref].\n\nTerm\n: Definition.\n\n[ref]: https://example.com\n",
        );
        if let Some(binary) = std::env::var_os("FANI_PR1_LEGACY_BINARY") {
            let output = Command::new(&binary)
                .args(["sync", "--config"])
                .arg(&fixture.config)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if trusted {
                let output = Command::new(binary)
                    .args(["adopt", "--config"])
                    .arg(&fixture.config)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        } else {
            fixture.sync(0);
            if trusted {
                fixture.adopt();
            }
        }
        let calls = fixture.calls();
        let original = fs::read(&fixture.target).unwrap();
        let conn = fixture.db().connect().unwrap();
        for table in ["units", "unit_versions"] {
            conn.execute(&format!("UPDATE {table} SET context_json=json_object('kind',json_extract(context_json,'$.kind'))"), []).unwrap();
        }
        conn.execute(
            "UPDATE attempts SET request_json=json_remove(request_json,'$._fani_unit')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE translation_memory_entries SET context_key=COALESCE((SELECT json_extract(uv.context_json,'$.kind') FROM translation_versions tv JOIN unit_versions uv ON uv.id=tv.unit_version_id WHERE tv.id=translation_version_id),context_key), provenance='legacy_fixture'", []).unwrap();
        if trusted {
            conn.execute("UPDATE canonical_candidates SET selected=0", [])
                .unwrap();
            conn.execute(
                "UPDATE translation_memory_entries SET superseded_at=1 WHERE tier='candidate'",
                [],
            )
            .unwrap();
        }
        let snapshots: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM attempts WHERE request_json LIKE '%_fani_unit%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(snapshots, 0);
        fixture.status(0, 3);
        fixture.sync(0);
        fixture.status(0, 3);
        fixture.sync(0);
        fixture.adopt();
        fixture.sync(0);
        assert_eq!(fixture.calls(), calls);
        assert_eq!(fs::read(&fixture.target).unwrap(), original);
        let legacy: i64 = conn.query_row("SELECT COUNT(*) FROM unit_versions WHERE json_extract(context_json,'$.parser_source') IS NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(legacy, 3);
        let bound: i64 = conn.query_row("SELECT COUNT(*) FROM translation_memory_entries WHERE provenance LIKE '%_fani_unit%'", [], |row| row.get(0)).unwrap();
        assert!(bound > 0);
    }
}

#[test]
fn reference_links_keep_document_context_through_sync_trust_and_adoption() {
    for source in [
        "Read [guide][ref].\n\n[ref]: https://example.com\n",
        include_str!("fixtures/markdown-corpus/rich-gfm.md"),
    ] {
        let fixture = Fixture::new(source);
        fs::write(
            &fixture.config,
            fs::read_to_string(&fixture.config)
                .unwrap()
                .replace("max_tasks = 10", "max_tasks = 100")
                .replace("enabled = false", "enabled = true"),
        )
        .unwrap();
        fixture.sync(0);
        let calls = fixture.calls();
        let db = fixture.db();
        let (repository, commit): (i64, String) = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT repository_id,candidate_commit FROM publication_manifests LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            db.promote_merged_publication(repository, "zh-CN", &commit, "verified_fixture")
                .unwrap()
                > 0
        );
        fixture.sync(0);
        assert_eq!(fixture.calls(), calls);
        fixture.adopt();
        fixture.sync(0);
        assert_eq!(fixture.calls(), calls);
        assert_eq!(
            fs::read_to_string(&fixture.target).unwrap(),
            source.replace("Hello", "Bonjour")
        );
    }
}

#[test]
fn changed_review_input_requeues_approval_within_the_unit_budget() {
    let fixture = Fixture::new("Hello `first`.\n\nHello `second`.\n");
    let config = fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("revision = false", "revision = true");
    fs::write(&fixture.config, format!("{config}revision = \"fixture\"\n")).unwrap();
    let provider = fixture.temp.path().join("provider.sh");
    fs::write(&provider, format!("#!/bin/sh\nset -eu\nprintf 'call\\n' >> '{}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:(if .task.stage == \"Revision\" then \"OK\" else (.task.source | gsub(\"Hello\";\"Bonjour\")) end)}}'\n", fixture.temp.path().join("calls").display())).unwrap();
    fixture.sync(0);
    assert_eq!(fixture.calls(), 4);
    fixture.adopt();
    let original = fs::read(&fixture.target).unwrap();
    let conn = fixture.db().connect().unwrap();
    conn.execute("UPDATE attempts SET request_json=json_set(request_json,'$.task.previous_translation','another candidate') WHERE dedupe_key LIKE '%:revision'", []).unwrap();
    fixture.budget_one();
    fixture.status(1, 0);
    fixture.sync(3);
    assert_eq!(fixture.calls(), 5);
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    fixture.status(1, 1);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 6);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 6);
    let retired: i64 = conn.query_row("SELECT COUNT(*) FROM attempts WHERE dedupe_key LIKE '%:revision:superseded:%' AND status='cancelled'", [], |row| row.get(0)).unwrap();
    assert_eq!(retired, 2);
    conn.execute("UPDATE attempts SET response_json=json_set(response_json,'$.output','Broken.') WHERE dedupe_key LIKE '%:translate' AND status='succeeded'", []).unwrap();
    conn.execute(
        "UPDATE canonical_candidates SET target_text='Broken.' WHERE selected=1",
        [],
    )
    .unwrap();
    conn.execute("UPDATE translation_versions SET target_text='Broken.' WHERE source_attempt_id IS NOT NULL AND superseded_at IS NULL", []).unwrap();
    conn.execute(
        "UPDATE translation_memory_entries SET tier='history',superseded_at=1 WHERE tier='trusted'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE translation_memory_entries SET target_text='Broken.' WHERE tier='candidate'",
        [],
    )
    .unwrap();
    fs::write(
        &provider,
        fs::read_to_string(&provider)
            .unwrap()
            .replace("Bonjour", "Salut"),
    )
    .unwrap();
    fixture.sync(3);
    assert_eq!(fixture.calls(), 8);
    assert_eq!(fs::read(&fixture.target).unwrap(), original);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 10);
    assert!(
        fs::read_to_string(&fixture.target)
            .unwrap()
            .contains("Salut")
    );
    fixture.sync(0);
    assert_eq!(fixture.calls(), 10);
}

#[test]
fn proofread_receipts_are_bound_to_the_exact_candidate() {
    let fixture = Fixture::new("Hello `code`.\n");
    let config = fs::read_to_string(&fixture.config)
        .unwrap()
        .replace("proofread = false", "proofread = true");
    fs::write(
        &fixture.config,
        format!("{config}proofread = \"fixture\"\n"),
    )
    .unwrap();
    fixture.sync(0);
    assert_eq!(fixture.calls(), 2);
    let conn = fixture.db().connect().unwrap();
    conn.execute("UPDATE attempts SET request_json=json_set(request_json,'$.task.previous_translation','different candidate') WHERE dedupe_key LIKE '%:proofread'", []).unwrap();
    fixture.status(1, 0);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 3);
    fixture.sync(0);
    assert_eq!(fixture.calls(), 3);
    let retired: i64 = conn.query_row("SELECT COUNT(*) FROM attempts WHERE dedupe_key LIKE '%:proofread:superseded:%' AND status='cancelled'", [], |row| row.get(0)).unwrap();
    assert_eq!(retired, 1);
}

#[test]
fn rejected_agent_and_deterministic_repairs_are_replaced_without_replaying_invalid_receipts() {
    for deterministic in [false, true] {
        let fixture = Fixture::new(if deterministic {
            "**Label:** text.\n"
        } else {
            "Hello `code`.\n"
        });
        let config = fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("repair_budget = 0", "repair_budget = 1");
        fs::write(&fixture.config, format!("{config}repair = \"fixture\"\n")).unwrap();
        let expression = if deterministic {
            "\"**标签：**文本。\""
        } else {
            "(if .task.stage == \"Repair\" then (.task.source | gsub(\"Hello\";\"Bonjour\")) else \"Broken.\" end)"
        };
        fs::write(fixture.temp.path().join("provider.sh"), format!("#!/bin/sh\nset -eu\nprintf 'call\\n' >> '{}'\njq -c '{{schema:\"fani.agent.response.v1\",task_id:.task.id,output:{expression}}}'\n", fixture.temp.path().join("calls").display())).unwrap();
        fixture.sync(0);
        let original = fs::read(&fixture.target).unwrap();
        let calls = fixture.calls();
        let conn = fixture.db().connect().unwrap();
        conn.execute("UPDATE attempts SET response_json=json_set(response_json,'$.output','Broken.') WHERE status='succeeded'", []).unwrap();
        conn.execute("UPDATE canonical_candidates SET target_text='Broken.'", [])
            .unwrap();
        conn.execute("UPDATE translation_versions SET target_text='Broken.'", [])
            .unwrap();
        conn.execute(
            "UPDATE translation_memory_entries SET target_text='Broken.'",
            [],
        )
        .unwrap();
        fixture.status(1, 0);
        fixture.sync(0);
        assert_eq!(fixture.calls(), calls + usize::from(!deterministic));
        assert_eq!(fs::read(&fixture.target).unwrap(), original);
        fixture.sync(0);
        assert_eq!(fixture.calls(), calls + usize::from(!deterministic));
        let retired: i64 = conn.query_row("SELECT COUNT(*) FROM attempts WHERE status='cancelled' AND dedupe_key LIKE '%:repair:%:superseded:%'", [], |row| row.get(0)).unwrap();
        assert_eq!(retired, 1);
    }
}

#[test]
fn both_merged_pr_entrypoints_check_current_project_rules_before_trust() {
    use fani::application::ports::PullRequestStateInput;
    for reconcile in [false, true] {
        let fixture = Fixture::new("Hello world.\n");
        let config = fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("enabled = false", "enabled = true");
        fs::write(&fixture.config, &config).unwrap();
        fixture.sync(0);
        let db = fixture.db();
        let conn = db.connect().unwrap();
        let (repository, commit): (i64, String) = conn
            .query_row(
                "SELECT repository_id,candidate_commit FROM publication_manifests LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let marker = fixture.temp.path().join("check-blocked");
        let bin = fixture.temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let pull = serde_json::json!({"number":1,"url":"https://example.invalid/pull/1","state":"MERGED","headRefName":"i18n/zh-CN","baseRefName":"main","isDraft":false,"title":"fixture","body":"fixture","headRefOid":commit});
        let response = fixture.temp.path().join("pull.json");
        fs::write(&response, pull.to_string()).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, format!("#!/bin/sh\nset -eu\ncase \"$1 $2\" in\n'pr list') printf '[]\\n';;\n'pr create') printf 'https://example.invalid/pull/1\\n';;\n'pr view') touch '{}'; cat '{}';;\n'pr edit') :;;\n*) exit 1;;\nesac\n", marker.display(), response.display())).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = config.replace("[agents.fixture]", "[repo.publish.github]\nenabled = true\nrepository = \"fixture/project\"\nbase = \"main\"\n[agents.fixture]");
        config = config.replace("[repo.quality]", &format!("[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"test ! -f '{}'\"]]\n[repo.quality]", marker.display()));
        config = config.replace(
            "source_ref = \"HEAD\"",
            "source_ref = \"HEAD\"\npush = true",
        );
        if reconcile {
            db.record_pr_state(PullRequestStateInput {
                repository_id: repository,
                provider: "github",
                external_id: "1",
                number: Some(1),
                branch: "i18n/zh-CN",
                url: Some("https://example.invalid/pull/1"),
                state: "open",
                head_revision: Some(&commit),
                event_key: "fixture",
                payload_json: "{}",
            })
            .unwrap();
        } else {
            let remote = fixture.temp.path().join("remote.git");
            fixture.git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
            fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
            conn.execute("UPDATE publication_outbox SET state='pending',owner=NULL,lease_expires_at=NULL,completed_at=NULL,available_at=0", []).unwrap();
        }
        fs::write(&fixture.config, config).unwrap();
        fixture.sync(if reconcile { 1 } else { 0 });
        assert!(marker.is_file());
        let state: String = conn
            .query_row(
                "SELECT state FROM publication_manifests WHERE candidate_commit=?1",
                [&commit],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "superseded");
        let trusted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM translation_memory_entries WHERE tier='trusted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trusted, 0);
        assert_eq!(fixture.calls(), 1);
    }
}

#[test]
fn pending_publication_revalidates_opaque_content_and_current_checks() {
    use sha2::{Digest, Sha256};
    for prepared in [false, true] {
        for change in ["heading", "metadata", "reference", "check"] {
            let fixture = Fixture::new(
                "---\nslug: stable\n---\n\n# Hello {#stable}\n\n[unused]: https://example.com/stable\n",
            );
            fs::write(
                &fixture.config,
                fs::read_to_string(&fixture.config)
                    .unwrap()
                    .replace("enabled = false", "enabled = true"),
            )
            .unwrap();
            fixture.sync(0);
            let original = fs::read(&fixture.target).unwrap();
            let conn = fixture.db().connect().unwrap();
            let (id, raw): (i64, String) = conn
                .query_row(
                    "SELECT id,payload_json FROM publication_outbox ORDER BY id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let mut payload: Value = serde_json::from_str(&raw).unwrap();
            if change == "check" {
                let config = fs::read_to_string(&fixture.config).unwrap();
                fs::write(&fixture.config, config.replace("[repo.quality]", "[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"exit 1\"]]\n[repo.quality]")).unwrap();
            } else {
                let content = payload["files"][0]["content"].as_str().unwrap();
                let content = if change == "heading" {
                    content.replace("{#stable}", "{#changed}")
                } else if change == "reference" {
                    content.replace("example.com/stable", "example.com/changed")
                } else {
                    content.replace("slug: stable", "slug: changed")
                };
                payload["files"][0]["content_hash"] =
                    format!("{:x}", Sha256::digest(content.as_bytes())).into();
                payload["files"][0]["content"] = content.into();
            }
            if !prepared {
                payload["commit"] = Value::Null;
            }
            conn.execute("UPDATE publication_outbox SET state='pending',owner=NULL,lease_expires_at=NULL,completed_at=NULL,available_at=0,payload_json=?2 WHERE id=?1", params![id,payload.to_string()]).unwrap();
            let output = fixture.run("sync");
            assert!(
                output.status.code() == Some(if change == "check" { 1 } else { 0 }),
                "{change}: {} {}",
                String::from_utf8_lossy(&output.stderr),
                fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap()
            );
            let (status, rejected): (String, String) = conn
                .query_row(
                    "SELECT state,payload_json FROM publication_outbox WHERE id=?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(status, "done");
            assert!(
                serde_json::from_str::<Value>(&rejected).unwrap()["superseded_reason"].is_string()
            );
            let state: String = conn
                .query_row(
                    "SELECT state FROM publication_manifests ORDER BY id LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if prepared {
                assert_eq!(state, "superseded");
            }
            let report: Value = serde_json::from_str(
                &fs::read_to_string(fixture.temp.path().join("reports/report.json")).unwrap(),
            )
            .unwrap();
            if change == "check" {
                assert_eq!(report["status"], "needs_human");
            }
            assert_eq!(report["languages"][0]["published"]["pushed"], false);
            assert_eq!(report["languages"][0]["written"], serde_json::json!([]));
            assert_eq!(fs::read(&fixture.target).unwrap(), original);
            assert_eq!(fixture.calls(), 1);
        }
    }
}

#[test]
fn compatible_adopted_publication_without_links_revalidates_reordered_code() {
    let fixture = Fixture::new("Use `first` before `second`.\n");
    fs::write(
        &fixture.config,
        fs::read_to_string(&fixture.config)
            .unwrap()
            .replace("enabled = false", "enabled = true"),
    )
    .unwrap();
    fixture.sync(0);
    let proposed = "Use `second` before `first`.\n";
    fs::write(&fixture.target, proposed).unwrap();
    fixture.adopt();
    let conn = fixture.db().connect().unwrap();
    let (id, raw): (i64, String) = conn
        .query_row(
            "SELECT id,payload_json FROM publication_outbox ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let (file, version, hash): (i64, i64, String) = conn
        .query_row(
            "SELECT id,current_content_version_id,content_hash FROM canonical_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let links: i64 = conn.query_row("SELECT COUNT(*) FROM canonical_file_translations WHERE canonical_content_version_id=?1", [version], |row| row.get(0)).unwrap();
    assert_eq!(links, 0);
    let mut payload: Value = serde_json::from_str(&raw).unwrap();
    payload["commit"] = Value::Null;
    payload["files"][0]["canonical_file_id"] = file.into();
    payload["files"][0]["canonical_content_version_id"] = version.into();
    payload["files"][0]["content"] = proposed.into();
    payload["files"][0]["content_hash"] = hash.into();
    // Adopted bytes need a new authorization, not a rewritten completed effect.
    conn.execute("INSERT INTO publication_outbox(repository_id,run_id,locale,dedupe_key,payload_json,available_at,created_at) SELECT repository_id,run_id,locale,dedupe_key||':adopted',?2,0,created_at FROM publication_outbox WHERE id=?1", params![id,payload.to_string()]).unwrap();
    let id = conn.last_insert_rowid();
    fixture.sync(0);
    let (state, raw): (String, String) = conn
        .query_row(
            "SELECT state,payload_json FROM publication_outbox WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "done");
    let payload: Value = serde_json::from_str(&raw).unwrap();
    assert!(payload["superseded_reason"].is_null());
    assert!(payload["commit"].is_string());
    assert_eq!(fixture.calls(), 1);
    assert_eq!(fs::read_to_string(&fixture.target).unwrap(), proposed);
}

#[test]
fn adoption_checks_exact_proposed_bytes_before_creating_trust() {
    let fixture = Fixture::new("Hello world.\n");
    fixture.sync(0);
    let proposed = "Salut world.\n";
    fs::write(&fixture.target, proposed).unwrap();
    let config = fs::read_to_string(&fixture.config).unwrap();
    let check = "[repo.documentation]\ncommands = [[\"sh\", \"-c\", \"grep -q Salut translations/zh-CN/docs/guide.md && exit 1\"]]\n[repo.quality]";
    fs::write(&fixture.config, config.replace("[repo.quality]", check)).unwrap();
    let output = fixture.run("adopt");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("documentation checks"));
    let conn = fixture.db().connect().unwrap();
    let trusted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM translation_memory_entries WHERE tier='trusted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trusted, 0);
    let tier: String = conn
        .query_row("SELECT trust_tier FROM canonical_files", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_ne!(tier, "trusted");
    assert_eq!(fixture.calls(), 1);
    assert_eq!(fs::read_to_string(&fixture.target).unwrap(), proposed);
    fs::write(
        &fixture.config,
        config.replace("[repo.quality]", &check.replace(" && exit 1", "")),
    )
    .unwrap();
    fixture.adopt();
    assert_eq!(fixture.calls(), 1);
}

#[test]
fn safe_inline_code_reorder_survives_adoption_and_trusted_reuse() {
    let fixture = Fixture::new("Use `first` before `second`.\n");
    fixture.sync(0);
    let translated = "Use `second` before `first`.\n";
    fs::write(&fixture.target, translated).unwrap();
    fixture.adopt();
    fixture.sync(0);
    assert_eq!(fixture.calls(), 1);
    assert_eq!(fs::read_to_string(&fixture.target).unwrap(), translated);
}

#[test]
fn adopt_rejects_opaque_frontmatter_changes_without_trusting_them() {
    for source in [
        "---\nslug: native-guide\n---\n\n# Hello\n",
        "+++\nslug = 'native-guide'\n+++\n\n# Hello\n",
        "Hello.\n\n[unused]: https://example.com/native-guide\n",
    ] {
        let fixture = Fixture::new(source);
        fixture.sync(0);
        let original = fs::read_to_string(&fixture.target).unwrap();
        fs::write(
            &fixture.target,
            original.replace("native-guide", "changed-guide"),
        )
        .unwrap();
        let rejected = fixture.run("adopt");
        assert_eq!(rejected.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("failed validation"));
        let count: i64 = fixture
            .db()
            .connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM translation_memory_entries WHERE tier='trusted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
