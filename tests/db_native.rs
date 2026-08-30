use fani::db::{AttemptInput, Database, OutboxKind, TrustTranslationInput};
use rusqlite::params;
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
