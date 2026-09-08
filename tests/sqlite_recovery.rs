use fani::test_support::db::Database;
use rusqlite::params;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fani(config: &Path, reports: &Path) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!("fani"))
        .args([
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--report-dir",
            reports.to_str().unwrap(),
            "--quiet",
        ])
        .output()
        .unwrap()
}

fn kill_at(config: &Path, reports: &Path, failpoint: &str, marker: &Path) {
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
        .env("FANI_TEST_FAILPOINT", failpoint)
        .env("FANI_TEST_FAILPOINT_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("fani exited before failpoint {failpoint}: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "fani did not reach failpoint {failpoint}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
}

fn marker(db: &Database) -> String {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT repository_key FROM repositories ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn open_migration_integrity_and_foreign_keys_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fani.db");
    let db = Database::open(&path).unwrap();
    assert_eq!(db.schema_version().unwrap(), 6);
    let conn = db.connect().unwrap();
    assert_eq!(
        conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
    let repository_id = db
        .upsert_repository("authority", temp.path(), Some("main"), None)
        .unwrap();
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    conn.execute(
        "INSERT INTO documents(repository_id,path,content_hash,created_at,updated_at) VALUES (?1,'orphan.md','hash',1,1)",
        params![repository_id + 1],
    )
    .unwrap();
    drop(conn);
    drop(db);

    let error = Database::open(&path).unwrap_err();
    assert!(error.to_string().contains("foreign key check"), "{error:#}");

    let corrupt = temp.path().join("corrupt.db");
    fs::write(&corrupt, b"not a SQLite database").unwrap();
    assert!(Database::open(&corrupt).is_err());
}

#[test]
fn online_backup_restore_is_atomic_and_rejects_corrupt_input() {
    let temp = tempfile::tempdir().unwrap();
    let authority_path = temp.path().join("authority.db");
    let backup_path = temp.path().join("backup.db");
    let restore_path = temp.path().join("restored.db");

    let authority = Database::open(&authority_path).unwrap();
    authority
        .upsert_repository("before", temp.path(), Some("main"), None)
        .unwrap();
    Database::snapshot(&authority_path, &backup_path).unwrap();
    authority
        .connect()
        .unwrap()
        .execute(
            "UPDATE repositories SET repository_key='after' WHERE repository_key='before'",
            [],
        )
        .unwrap();

    let destination = Database::open(&restore_path).unwrap();
    destination
        .upsert_repository("destination", &restore_path, None, None)
        .unwrap();
    drop(destination);
    let restored = Database::restore(&backup_path, &restore_path).unwrap();
    assert_eq!(marker(&restored), "before");
    restored.integrity_check().unwrap();

    let corrupt_backup = temp.path().join("corrupt-backup.db");
    fs::write(&corrupt_backup, b"broken backup").unwrap();
    let error = Database::restore(&corrupt_backup, &restore_path).unwrap_err();
    assert!(error.to_string().contains("cannot restore database"));
    assert_eq!(marker(&Database::open(&restore_path).unwrap()), "before");
}

#[test]
fn stale_and_dead_leases_are_recovered_with_fencing() {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(temp.path().join("fani.db")).unwrap();
    let stale = db
        .acquire_lease("repository", "repo", "worker-a", 1_000, 100)
        .unwrap()
        .unwrap();
    assert!(
        db.acquire_lease("repository", "repo", "worker-b", 1_099, 100)
            .unwrap()
            .is_none()
    );
    let recovered = db
        .acquire_lease("repository", "repo", "worker-b", 1_100, 100)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.fencing_token, stale.fencing_token + 1);
    assert!(!db.release_lease(&stale).unwrap());

    let dead = db
        .acquire_lease(
            "repository",
            "dead-repo",
            "4294967295:1:crashed",
            2_000,
            60_000,
        )
        .unwrap()
        .unwrap();
    let live = db
        .acquire_lease(
            "repository",
            "dead-repo",
            &format!("{}:1:new", std::process::id()),
            2_001,
            60_000,
        )
        .unwrap()
        .unwrap();
    assert_eq!(live.fencing_token, dead.fencing_token + 1);
}

struct CrashFixture {
    _temp: TempDir,
    repo: PathBuf,
    config: PathBuf,
    reports: PathBuf,
    counter: PathBuf,
    marker: PathBuf,
    target: PathBuf,
}

impl CrashFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("docs/guide.md"), "# Hello\n").unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "initial"]);

        let counter = temp.path().join("provider-count");
        let provider = temp.path().join("provider.sh");
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

        let config = temp.path().join("fani.toml");
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
                repo.display(),
                provider.display()
            ),
        )
        .unwrap();
        let reports = temp.path().join("reports");
        let marker = temp.path().join("failpoint");
        let target = repo.join("translations/zh-CN/docs/guide.md");
        Self {
            _temp: temp,
            repo,
            config,
            reports,
            counter,
            marker,
            target,
        }
    }

    fn provider_calls(&self) -> String {
        fs::read_to_string(&self.counter).unwrap()
    }

    fn update_source(&self, source: &str) {
        fs::write(self.repo.join("docs/guide.md"), source).unwrap();
        git(&self.repo, &["add", "docs/guide.md"]);
        git(&self.repo, &["commit", "-qm", source.trim()]);
    }

    fn recover(&self, expected_calls: &str) {
        let output = fani(&self.config, &self.reports);
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(self.provider_calls(), expected_calls);
    }
}

#[test]
fn zero_unit_crash_windows_recover_without_fake_translation_history() {
    for failpoint in [
        "project_checks_completed",
        "canonical_content_before_translation_links",
        "canonical_persisted_before_outbox",
        "materialization_before_file_write",
        "materialized_file_written",
        "materialization_state_transitioned",
        "publication_candidate_persisted",
        "publication_side_effect_completed",
    ] {
        let fixture = CrashFixture::new();
        let source = "<!-- pass-through -->\r\n\r\n```text\r\nopaque\r\n```\r\n";
        fixture.update_source(source);
        fs::write(&fixture.counter, "0").unwrap();
        fs::write(
            &fixture.config,
            fs::read_to_string(&fixture.config)
                .unwrap()
                .replace("enabled = false", "enabled = true"),
        )
        .unwrap();
        kill_at(
            &fixture.config,
            &fixture.reports,
            failpoint,
            &fixture.marker,
        );
        fixture.recover("0");
        fixture.recover("0");
        assert_eq!(
            fs::read(&fixture.target).unwrap(),
            source.as_bytes(),
            "{failpoint}"
        );
        let db = Database::open(fixture.repo.join(".fani/fani.db")).unwrap();
        let conn = db.connect().unwrap();
        for table in [
            "units",
            "attempts",
            "translation_versions",
            "translation_memory_entries",
            "canonical_file_translations",
        ] {
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "{failpoint}: {table}"
            );
        }
        for table in ["materialization_outbox", "publication_outbox"] {
            assert_eq!(
                conn.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE state<>'done'"),
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                0,
                "{failpoint}: {table}"
            );
            assert_eq!(
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1,
                "{failpoint}: {table}"
            );
        }
    }
}

#[test]
fn cli_crash_recovery_preserves_atomicity_and_never_duplicates_provider_calls() {
    let fixture = CrashFixture::new();

    kill_at(
        &fixture.config,
        &fixture.reports,
        "canonical_content_before_translation_links",
        &fixture.marker,
    );
    assert_eq!(fixture.provider_calls(), "1");
    assert!(!fixture.target.exists());
    let db = Database::open(fixture.repo.join(".fani/fani.db")).unwrap();
    let rolled_back: (i64, i64, i64) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM canonical_files),(SELECT COUNT(*) FROM canonical_content_versions),(SELECT COUNT(*) FROM canonical_file_translations)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(rolled_back, (0, 0, 0));
    fixture.recover("1");
    assert_eq!(fs::read_to_string(&fixture.target).unwrap(), "# 你好\n");

    fixture.update_source("# Hello before-side-effect\n");
    kill_at(
        &fixture.config,
        &fixture.reports,
        "materialization_before_file_write",
        &fixture.marker,
    );
    assert_eq!(fixture.provider_calls(), "2");
    assert_eq!(fs::read_to_string(&fixture.target).unwrap(), "# 你好\n");
    fixture.recover("2");
    assert_eq!(
        fs::read_to_string(&fixture.target).unwrap(),
        "# 你好 before-side-effect\n"
    );

    fixture.update_source("# Hello after-file-side-effect\n");
    kill_at(
        &fixture.config,
        &fixture.reports,
        "materialized_file_written",
        &fixture.marker,
    );
    assert_eq!(fixture.provider_calls(), "3");
    assert_eq!(
        fs::read_to_string(&fixture.target).unwrap(),
        "# 你好 after-file-side-effect\n"
    );
    fixture.recover("3");

    fixture.update_source("# Hello after-state-transition\n");
    kill_at(
        &fixture.config,
        &fixture.reports,
        "materialization_state_transitioned",
        &fixture.marker,
    );
    assert_eq!(fixture.provider_calls(), "4");
    assert_eq!(
        fs::read_to_string(&fixture.target).unwrap(),
        "# 你好 after-state-transition\n"
    );
    let before_recovery: (String, String) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT cf.state,mo.state FROM canonical_files cf JOIN materialization_outbox mo ON json_extract(mo.payload_json,'$.path')=cf.path ORDER BY mo.id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        before_recovery,
        ("materialized".into(), "processing".into())
    );
    fixture.recover("4");

    let conn = db.connect().unwrap();
    let final_state: (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM attempts WHERE status='succeeded'),(SELECT COUNT(*) FROM canonical_content_versions),(SELECT COUNT(*) FROM canonical_file_translations),(SELECT COUNT(*) FROM materialization_outbox WHERE state!='done')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(final_state, (4, 4, 4, 0));
    drop(conn);
    db.integrity_check().unwrap();
}
