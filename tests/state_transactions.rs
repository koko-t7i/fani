use fani::application::ports::{
    CanonicalStore, DocumentStore, EffectStore, MaterializationIntentInput, OutboxKind,
};
use fani::domain::{
    model::{
        Freshness, MemoryTier, PublicationState, ReviewState, TranslationProvenance,
        ValidationState,
    },
    prompts,
};
use fani::test_support::db::{CanonicalFileInput, Database};
use serde_json::json;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    db: Database,
    repository: i64,
    document: i64,
    run: String,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(temp.path().join("fani.db")).unwrap();
    let repository = db
        .upsert_repository("docs", temp.path(), None, None)
        .unwrap();
    let document = db
        .upsert_document(repository, "guide.md", Some("revision"), "hash", "{}")
        .unwrap();
    let run = db
        .begin_run(
            repository,
            "invocation",
            &temp.path().join("fani.toml"),
            "{}",
            &prompts::policy_fingerprint(),
        )
        .unwrap();
    Fixture {
        _temp: temp,
        db,
        repository,
        document,
        run,
    }
}

fn persist(
    f: &Fixture,
    content: &str,
    identity: &str,
) -> anyhow::Result<fani::application::ports::CanonicalFile> {
    f.db.persist_canonical_document(
        CanonicalFileInput {
            repository_id: f.repository,
            locale: "fr",
            path: "fr/guide.md",
            source_revision: "revision",
            content: content.as_bytes(),
            content_hash: content,
            materialized_hash: None,
            freshness: Freshness::Exact,
            provenance: TranslationProvenance::Imported,
            validation: ValidationState::Passed,
            review: ReviewState::Unreviewed,
            publication: PublicationState::Candidate,
            trust_tier: MemoryTier::Candidate,
            policy_fingerprint: &prompts::policy_fingerprint(),
        },
        &[],
        identity,
    )
}

fn schedule(
    f: &Fixture,
    key: &str,
) -> anyhow::Result<fani::application::ports::MaterializationReceipt> {
    f.db.schedule_materialization(MaterializationIntentInput {
        repository_id: f.repository,
        run_id: &f.run,
        document_id: f.document,
        locale: "fr",
        path: "fr/guide.md",
        dedupe_key: key,
        work_input_json: &json!({"effect_key":key}).to_string(),
        payload_json: &json!({"repository_id":f.repository,"locale":"fr","path":"fr/guide.md"})
            .to_string(),
    })
}

#[test]
fn read_only_repository_lookup_does_not_register_unknown_repository() {
    let f = fixture();
    assert_eq!(f.db.repository_id("missing").unwrap(), None);
    assert_eq!(f.db.repository_id("docs").unwrap(), Some(f.repository));
    let count: i64 =
        f.db.connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM repositories", [], |row| row.get(0))
            .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn incompatible_document_identity_rolls_back_content_and_version_selection() {
    let f = fixture();
    let identity =
        json!({"source_revision":"revision","target_path":"fr/guide.md","locale":"fr"}).to_string();
    let first = persist(&f, "original", &identity).unwrap();
    let incompatible =
        json!({"source_revision":"other-revision","target_path":"fr/guide.md","locale":"fr"})
            .to_string();
    assert!(
        persist(&f, "replacement", &incompatible)
            .unwrap_err()
            .to_string()
            .contains("identity conflicts")
    );
    let canonical =
        f.db.canonical_file(f.repository, "fr", "fr/guide.md")
            .unwrap()
            .unwrap();
    assert_eq!(canonical.content_version_id, first.content_version_id);
    assert_eq!(canonical.content, b"original");
    assert_eq!(
        f.db.canonical_document_intent(first.content_version_id)
            .unwrap()
            .as_deref(),
        Some(identity.as_str())
    );
    let count: i64 =
        f.db.connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM canonical_content_versions",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(count, 1);
    f.db.integrity_check().unwrap();
}

#[test]
fn outbox_failure_rolls_back_supersession_and_new_work() {
    let f = fixture();
    let old = schedule(&f, "old").unwrap();
    let conn = f.db.connect().unwrap();
    conn.execute_batch("CREATE TRIGGER reject_new_intent BEFORE INSERT ON materialization_outbox WHEN NEW.dedupe_key='new' BEGIN SELECT RAISE(ABORT,'injected outbox failure'); END;").unwrap();
    assert!(
        schedule(&f, "new")
            .unwrap_err()
            .to_string()
            .contains("injected outbox failure")
    );
    let state: (String, String) = conn.query_row("SELECT o.state,w.status FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id WHERE o.id=?1", [old.outbox_id], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(state, ("pending".into(), "pending".into()));
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    conn.execute_batch("DROP TRIGGER reject_new_intent;")
        .unwrap();
    let replacement = schedule(&f, "new").unwrap();
    assert_ne!(replacement.work_item_id, old.work_item_id);
    assert_eq!(schedule(&f, "new").unwrap(), replacement);
    let state: (String, String) = conn.query_row("SELECT o.state,w.status FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id WHERE o.id=?1", [old.outbox_id], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(state, ("done".into(), "cancelled".into()));
    f.db.integrity_check().unwrap();
}

#[test]
fn terminal_receipts_require_a_new_effect_key() {
    let f = fixture();
    let old = schedule(&f, "materialize").unwrap();
    f.db.connect()
        .unwrap()
        .execute(
            "UPDATE materialization_outbox SET state='done',completed_at=1 WHERE id=?1",
            [old.outbox_id],
        )
        .unwrap();
    assert!(
        schedule(&f, "materialize")
            .unwrap_err()
            .to_string()
            .contains("already terminal")
    );
    let key =
        f.db.effect_key(OutboxKind::Materialization, "materialize")
            .unwrap();
    let new = schedule(&f, &key).unwrap();
    assert_ne!(new.outbox_id, old.outbox_id);
    assert_eq!(schedule(&f, &key).unwrap(), new);
}

#[test]
fn a_foreign_effect_key_cannot_reuse_another_repositories_outbox() {
    let f = fixture();
    let old = schedule(&f, "shared-key").unwrap();
    let other =
        f.db.upsert_repository("other", &f._temp.path().join("other"), None, None)
            .unwrap();
    let document =
        f.db.upsert_document(other, "guide.md", Some("revision"), "hash", "{}")
            .unwrap();
    let run =
        f.db.begin_run(
            other,
            "other",
            &f._temp.path().join("fani.toml"),
            "{}",
            &prompts::policy_fingerprint(),
        )
        .unwrap();
    let result = f.db.schedule_materialization(MaterializationIntentInput {
        repository_id: other,
        run_id: &run,
        document_id: document,
        locale: "fr",
        path: "fr/guide.md",
        dedupe_key: "shared-key",
        work_input_json: r#"{"effect_key":"shared-key"}"#,
        payload_json: &json!({"repository_id":other,"locale":"fr","path":"fr/guide.md"})
            .to_string(),
    });
    assert!(result.unwrap_err().to_string().contains("conflicts"));
    assert_eq!(schedule(&f, "shared-key").unwrap(), old);
    let count: i64 =
        f.db.connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
            .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn pending_receipt_keeps_original_payload_when_resumed_by_a_new_run() {
    let f = fixture();
    let old = schedule(&f, "pending").unwrap();
    let run =
        f.db.begin_run(
            f.repository,
            "resumed",
            &f._temp.path().join("fani.toml"),
            "{}",
            &prompts::policy_fingerprint(),
        )
        .unwrap();
    let resumed = f.db.schedule_materialization(MaterializationIntentInput {
        repository_id: f.repository, run_id: &run, document_id: f.document,
        locale: "fr", path: "fr/guide.md", dedupe_key: "pending",
        work_input_json: r#"{"effect_key":"pending"}"#,
        payload_json: &json!({"repository_id":f.repository,"locale":"fr","path":"fr/guide.md","canonical_content_version_id":42,"document_identity":{"source_revision":"new-revision"}}).to_string(),
    }).unwrap();
    assert_eq!(resumed, old);
    let stored: (String, String) = f.db.connect().unwrap().query_row(
        "SELECT o.payload_json,w.run_id FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id WHERE o.id=?1",
        [old.outbox_id], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored.0).unwrap(),
        json!({"repository_id":f.repository,"locale":"fr","path":"fr/guide.md"})
    );
    assert_eq!(stored.1, f.run);
}
