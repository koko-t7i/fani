use fani::test_support::db::{
    AttemptCandidateInput, AttemptInput, Database, OutboxKind, TrustTranslationInput,
};
use rusqlite::params;
use std::fs;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    db: Database,
    repository_id: i64,
    unit_id: i64,
    run_id: String,
    work_item_id: i64,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(temp.path().join("fani.db")).unwrap();
    let repository_id = db
        .upsert_repository("docs", &temp.path().join("repo"), Some("main"), None)
        .unwrap();
    let document_id = db
        .upsert_document(repository_id, "guide.md", Some("abc123"), "doc-hash", "{}")
        .unwrap();
    let unit_id = db
        .upsert_unit(
            document_id,
            "heading:intro",
            0,
            "Introduction",
            "source-hash",
            "{}",
        )
        .unwrap();
    let run_id = db
        .begin_run(
            repository_id,
            "sync:abc123:zh-CN",
            temp.path().join("fani.toml").as_path(),
            "{}",
        )
        .unwrap();
    let work_item_id = db
        .enqueue_work_item(&run_id, unit_id, "zh-CN", "translate", 10, "{}")
        .unwrap();
    Fixture {
        _temp: temp,
        db,
        repository_id,
        unit_id,
        run_id,
        work_item_id,
    }
}

#[test]
fn schema_enforces_pragmas_integrity_and_foreign_keys() {
    let fixture = fixture();
    let conn = fixture.db.connect().unwrap();
    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    let synchronous: i64 = conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .unwrap();
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let busy_timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();

    assert_eq!(foreign_keys, 1);
    assert_eq!(synchronous, 2);
    assert_eq!(journal_mode.to_ascii_lowercase(), "delete");
    assert_eq!(busy_timeout, 10_000);

    let invalid_child = conn.execute(
        "INSERT INTO documents(repository_id,path,content_hash,created_at,updated_at) VALUES (?1,'orphan.md','x',1,1)",
        params![fixture.repository_id + 10_000],
    );
    assert!(invalid_child.is_err());
    fixture.db.integrity_check().unwrap();
}

#[test]
fn fresh_and_latest_databases_verify_embedded_migration_metadata() {
    use sha2::{Digest, Sha256};

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fani.db");
    let db = Database::open(&path).unwrap();
    let conn = db.connect().unwrap();
    let application_id: i64 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .unwrap();
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let migration: (i64, String, String) = conn
        .query_row(
            "SELECT version,name,checksum FROM schema_migrations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let expected = format!(
        "{:x}",
        Sha256::digest(include_str!("../migrations/0001_native_authority.sql").as_bytes())
    );
    assert_eq!(application_id, 0x4641_4e49);
    assert_eq!(user_version, 1);
    assert_eq!(migration, (1, "0001_native_authority".into(), expected));
    drop(conn);
    drop(db);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 1);
    let count: i64 = reopened
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn migration_checksum_tampering_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fani.db");
    let db = Database::open(&path).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE schema_migrations SET checksum=?1 WHERE version=1",
            ["0".repeat(64)],
        )
        .unwrap();
    drop(db);

    let error = Database::open(&path).unwrap_err();
    assert!(
        error.to_string().contains("checksum/name mismatch"),
        "{error:#}"
    );
}

#[test]
fn reset_only_reinitializes_identified_fani_databases() {
    let temp = tempfile::tempdir().unwrap();
    let fani_path = temp.path().join("fani.db");
    let db = Database::open(&fani_path).unwrap();
    db.upsert_repository("docs", &temp.path().join("repo"), None, None)
        .unwrap();
    drop(db);

    let reset = Database::reset(&fani_path).unwrap();
    let repositories: i64 = reset
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM repositories", [], |row| row.get(0))
        .unwrap();
    assert_eq!(repositories, 0);

    let unrelated_path = temp.path().join("unrelated.db");
    let unrelated = rusqlite::Connection::open(&unrelated_path).unwrap();
    unrelated
        .execute("CREATE TABLE personal_data(value TEXT)", [])
        .unwrap();
    drop(unrelated);
    assert!(
        Database::open(&unrelated_path)
            .unwrap_err()
            .to_string()
            .contains("unsupported pre-native")
    );
    assert!(
        Database::reset(&unrelated_path)
            .unwrap_err()
            .to_string()
            .contains("refusing to reset")
    );
    assert!(unrelated_path.exists());
}

#[test]
fn attempt_recording_is_idempotent_by_work_item_and_dedupe_key() {
    let fixture = fixture();
    let first = fixture
        .db
        .record_attempt(AttemptInput {
            work_item_id: fixture.work_item_id,
            dedupe_key: "dispatch:heading:intro:1",
            agent: "translator",
            status: "succeeded",
            request_json: r#"{"prompt":"translate"}"#,
            response_json: Some(r#"{"text":"介绍"}"#),
            error: None,
        })
        .unwrap();
    let replay = fixture
        .db
        .record_attempt(AttemptInput {
            work_item_id: fixture.work_item_id,
            dedupe_key: "dispatch:heading:intro:1",
            agent: "translator",
            status: "failed",
            request_json: r#"{"prompt":"different replay"}"#,
            response_json: None,
            error: Some("must not overwrite the durable first result"),
        })
        .unwrap();

    assert!(first.inserted);
    assert!(!replay.inserted);
    assert_eq!(replay.id, first.id);
    let conn = fixture.db.connect().unwrap();
    let row: (i64, String, String) = conn
        .query_row(
            "SELECT COUNT(*),status,response_json FROM attempts WHERE work_item_id=?1",
            [fixture.work_item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row.0, 1);
    assert_eq!(row.1, "succeeded");
    assert_eq!(row.2, r#"{"text":"介绍"}"#);
}

#[test]
fn successful_attempt_and_candidate_commit_atomically_and_are_recoverable() {
    let fixture = fixture();
    let response = r#"{"output":"介绍"}"#;
    let receipt = fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "translate:heading:intro",
                agent: "translator",
                status: "succeeded",
                request_json: r#"{"prompt":"translate"}"#,
                response_json: Some(response),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:translate:heading:intro",
            target_text: "介绍",
            score: Some(1.0),
        })
        .unwrap();

    let recovered = fixture
        .db
        .successful_attempt(fixture.work_item_id, "translate:heading:intro")
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, receipt.id);
    assert_eq!(recovered.output, "介绍");
    assert_eq!(
        fixture
            .db
            .selected_candidate(fixture.unit_id, "zh-CN")
            .unwrap()
            .as_deref(),
        Some("介绍")
    );

    let replay = fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "translate:heading:intro",
                agent: "translator",
                status: "succeeded",
                request_json: r#"{"prompt":"translate"}"#,
                response_json: Some(response),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:translate:heading:intro",
            target_text: "介绍",
            score: Some(1.0),
        })
        .unwrap();
    assert!(!replay.inserted);
    assert_eq!(replay.id, receipt.id);
}

#[test]
fn expired_outbox_claims_are_recovered_and_replayed_once() {
    let fixture = fixture();
    let now = chrono::Utc::now().timestamp_millis();
    let id = fixture
        .db
        .enqueue_materialization(
            fixture.work_item_id,
            "materialize:guide.md:zh-CN:abc123",
            r#"{"path":"zh-CN/guide.md"}"#,
        )
        .unwrap();

    let first = fixture
        .db
        .claim_outbox(OutboxKind::Materialization, "worker-a", now, 100)
        .unwrap()
        .unwrap();
    assert_eq!(first.id, id);
    assert_eq!(first.attempt_count, 1);
    assert!(
        fixture
            .db
            .claim_outbox(OutboxKind::Materialization, "worker-b", now + 50, 100)
            .unwrap()
            .is_none()
    );

    let recovered = fixture
        .db
        .claim_outbox(OutboxKind::Materialization, "worker-b", now + 101, 100)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, id);
    assert_eq!(recovered.attempt_count, 2);
    assert!(
        !fixture
            .db
            .complete_outbox(OutboxKind::Materialization, id, "worker-a")
            .unwrap()
    );
    assert!(
        fixture
            .db
            .complete_outbox(OutboxKind::Materialization, id, "worker-b")
            .unwrap()
    );
    assert!(
        fixture
            .db
            .claim_outbox(OutboxKind::Materialization, "worker-c", now + 1_000, 100)
            .unwrap()
            .is_none()
    );

    let publication_id = fixture
        .db
        .enqueue_publication(
            fixture.repository_id,
            Some(&fixture.run_id),
            "zh-CN",
            "publish:zh-CN:abc123",
            r#"{"commit":"abc123"}"#,
        )
        .unwrap();
    let publication = fixture
        .db
        .claim_outbox(OutboxKind::Publication, "publisher", now + 1_000, 100)
        .unwrap()
        .unwrap();
    assert_eq!(publication.id, publication_id);
    assert!(
        fixture
            .db
            .retry_outbox(
                OutboxKind::Publication,
                publication_id,
                "publisher",
                "network unavailable",
                now + 1_500,
            )
            .unwrap()
    );
    assert!(
        fixture
            .db
            .claim_outbox(OutboxKind::Publication, "publisher", now + 1_499, 100)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .db
            .claim_outbox(OutboxKind::Publication, "publisher", now + 1_500, 100)
            .unwrap()
            .unwrap()
            .attempt_count,
        2
    );
}

#[test]
fn newer_materialization_supersedes_stale_processing_content() {
    let fixture = fixture();
    let now = chrono::Utc::now().timestamp_millis();
    let stale_key = "materialize:guide.md:zh-CN:old";
    let active_key = "materialize:guide.md:zh-CN:new";
    fixture
        .db
        .enqueue_materialization(
            fixture.work_item_id,
            stale_key,
            r#"{"locale":"zh-CN","path":"zh-CN/guide.md"}"#,
        )
        .unwrap();
    fixture
        .db
        .claim_outbox_key(
            OutboxKind::Materialization,
            stale_key,
            "materialize:999999999:1:dead",
            now,
            60_000,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture
            .db
            .supersede_materializations("zh-CN", "zh-CN/guide.md", active_key)
            .unwrap(),
        1
    );
    fixture
        .db
        .enqueue_materialization(
            fixture.work_item_id,
            active_key,
            r#"{"locale":"zh-CN","path":"zh-CN/guide.md"}"#,
        )
        .unwrap();
    let conn = fixture.db.connect().unwrap();
    let states: (String, String) = conn
        .query_row(
            "SELECT (SELECT state FROM materialization_outbox WHERE dedupe_key=?1),(SELECT state FROM materialization_outbox WHERE dedupe_key=?2)",
            [stale_key, active_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(states, ("done".into(), "pending".into()));
    drop(conn);

    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let started_at = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap();
    fixture
        .db
        .claim_outbox_key(
            OutboxKind::Materialization,
            active_key,
            &format!("materialize:{pid}:{started_at}:live"),
            chrono::Utc::now().timestamp_millis(),
            60_000,
        )
        .unwrap()
        .unwrap();
    let error = fixture
        .db
        .supersede_materializations("zh-CN", "zh-CN/guide.md", "newest")
        .unwrap_err();
    assert!(error.to_string().contains("live worker"), "{error:#}");
}

#[test]
fn leases_exclude_live_owners_and_fence_expired_owners() {
    let fixture = fixture();
    let first = fixture
        .db
        .acquire_lease("repository", "docs", "worker-a", 1_000, 100)
        .unwrap()
        .unwrap();
    assert_eq!(first.fencing_token, 1);
    assert!(
        fixture
            .db
            .acquire_lease("repository", "docs", "worker-b", 1_050, 100)
            .unwrap()
            .is_none()
    );

    let renewed = fixture.db.renew_lease(&first, 1_050, 200).unwrap().unwrap();
    assert_eq!(renewed.fencing_token, 1);
    assert_eq!(renewed.expires_at, 1_250);
    assert!(
        fixture
            .db
            .acquire_lease("repository", "docs", "worker-b", 1_249, 100)
            .unwrap()
            .is_none()
    );

    let takeover = fixture
        .db
        .acquire_lease("repository", "docs", "worker-b", 1_250, 100)
        .unwrap()
        .unwrap();
    assert_eq!(takeover.fencing_token, 2);
    assert!(!fixture.db.release_lease(&renewed).unwrap());
    assert!(
        fixture
            .db
            .renew_lease(&renewed, 1_251, 100)
            .unwrap()
            .is_none()
    );
    assert!(fixture.db.release_lease(&takeover).unwrap());
}

#[test]
fn canonical_selection_and_trusted_tm_have_single_authoritative_rows() {
    let fixture = fixture();
    let attempt = fixture
        .db
        .record_attempt(AttemptInput {
            work_item_id: fixture.work_item_id,
            dedupe_key: "candidate-1",
            agent: "translator",
            status: "succeeded",
            request_json: "{}",
            response_json: Some("{}"),
            error: None,
        })
        .unwrap();
    fixture
        .db
        .select_canonical_candidate(
            fixture.unit_id,
            "zh-CN",
            "candidate-a",
            "介绍",
            Some(attempt.id),
            Some(0.8),
        )
        .unwrap();
    fixture
        .db
        .select_canonical_candidate(
            fixture.unit_id,
            "zh-CN",
            "candidate-b",
            "简介",
            Some(attempt.id),
            Some(0.9),
        )
        .unwrap();
    let first_tm = fixture
        .db
        .trust_translation(TrustTranslationInput {
            repository_id: fixture.repository_id,
            unit_id: Some(fixture.unit_id),
            locale: "zh-CN",
            source_hash: "source-hash",
            context_key: "heading",
            target_text: "介绍",
            provenance: "reviewed",
        })
        .unwrap();
    let replay_tm = fixture
        .db
        .trust_translation(TrustTranslationInput {
            repository_id: fixture.repository_id,
            unit_id: Some(fixture.unit_id),
            locale: "zh-CN",
            source_hash: "source-hash",
            context_key: "heading",
            target_text: "简介",
            provenance: "approved",
        })
        .unwrap();

    assert_eq!(first_tm, replay_tm);
    let conn = fixture.db.connect().unwrap();
    let selected: (i64, String) = conn
        .query_row(
            "SELECT COUNT(*),target_text FROM canonical_candidates WHERE unit_id=?1 AND locale='zh-CN' AND selected=1",
            [fixture.unit_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(selected, (1, "简介".into()));
    let tm: (i64, String, String) = conn
        .query_row(
            "SELECT COUNT(*),target_text,provenance FROM trusted_translation_memory WHERE repository_id=?1 AND locale='zh-CN'",
            [fixture.repository_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(tm, (1, "简介".into(), "approved".into()));
    fixture.db.integrity_check().unwrap();
}
