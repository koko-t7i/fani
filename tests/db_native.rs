use fani::domain::{
    model::{
        CanonicalTransition, Freshness, MemoryTier, PublicationState, ReviewState,
        TranslationProvenance, ValidationState,
    },
    prompts,
};
use fani::test_support::db::{
    AttemptCandidateInput, AttemptInput, CanonicalFileInput, CanonicalTranslationInput, Database,
    OutboxKind, PublicationManifestFile, PublicationManifestInput, TrustTranslationInput,
};
use rusqlite::params;
use std::fs;
use tempfile::TempDir;

const TEST_FINGERPRINT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
            &fani::domain::document::unit_metadata(
                &fani::domain::markdown::extract_units("Introduction")[0],
                "guide.md",
            ),
        )
        .unwrap();
    let run_id = db
        .begin_run(
            repository_id,
            "sync:abc123:zh-CN",
            temp.path().join("fani.toml").as_path(),
            "{}",
            &prompts::policy_fingerprint(),
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
fn document_work_is_unique_scoped_and_creates_no_translation_records() {
    let fixture = fixture();
    let store = &fixture.db;
    let document_id = fixture
        .db
        .upsert_document(
            fixture.repository_id,
            "empty.md",
            Some("abc123"),
            "empty",
            "{}",
        )
        .unwrap();
    let first = store
        .enqueue_document_work_item(&fixture.run_id, document_id, "fr", "assembly", 1, "{}")
        .unwrap();
    let repeated = store
        .enqueue_document_work_item(
            &fixture.run_id,
            document_id,
            "fr",
            "assembly",
            2,
            r#"{"retry":true}"#,
        )
        .unwrap();
    assert_eq!(first, repeated);
    let other = store
        .enqueue_document_work_item(
            &fixture.run_id,
            document_id,
            "fr",
            "materialization",
            1,
            "{}",
        )
        .unwrap();
    assert_ne!(first, other);
    assert!(
        store
            .enqueue_document_work_item(&fixture.run_id, document_id, "fr", "translate", 1, "{}")
            .is_err()
    );
    assert!(
        store
            .enqueue_document_work_item(
                &fixture.run_id,
                document_id + 1000,
                "fr",
                "assembly",
                1,
                "{}"
            )
            .is_err()
    );
    let conn = fixture.db.connect().unwrap();
    assert!(
        conn.execute(
            "UPDATE work_items SET unit_id=?1 WHERE id=?2",
            params![fixture.unit_id, first]
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE work_items SET document_id=NULL WHERE id=?1",
            [first]
        )
        .is_err()
    );
    assert_eq!(
        conn.query_row(
            "SELECT unit_id,document_id,priority,input_json FROM work_items WHERE id=?1",
            [first],
            |row| Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?
            ))
        )
        .unwrap(),
        (None, document_id, 2, r#"{"retry":true}"#.into())
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM units WHERE document_id=?1",
            [document_id],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    for table in [
        "attempts",
        "translation_versions",
        "translation_memory_entries",
    ] {
        assert_eq!(
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    let unit_work = fixture
        .db
        .enqueue_work_item(
            &fixture.run_id,
            fixture.unit_id,
            "zh-CN",
            "translate",
            1,
            "{}",
        )
        .unwrap();
    assert_eq!(unit_work, fixture.work_item_id);
    let outbox = fixture
        .db
        .enqueue_materialization(other, "empty-document", "{}")
        .unwrap();
    assert_eq!(
        outbox,
        fixture
            .db
            .enqueue_materialization(other, "empty-document", "{}")
            .unwrap()
    );
    fixture.db.integrity_check().unwrap();
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
    let migrations = {
        let mut statement = conn
            .prepare("SELECT version,name,checksum FROM schema_migrations ORDER BY version")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let expected = [
        (
            1,
            "0001_native_authority".to_string(),
            format!(
                "{:x}",
                Sha256::digest(include_str!("../migrations/0001_native_authority.sql").as_bytes())
            ),
        ),
        (
            2,
            "0002_orthogonal_translation_state".to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    include_str!("../migrations/0002_orthogonal_translation_state.sql").as_bytes()
                )
            ),
        ),
        (
            3,
            "0003_document_work_items".to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    include_str!("../migrations/0003_document_work_items.sql").as_bytes()
                )
            ),
        ),
        (
            4,
            "0004_revision_bound_canonical_content".to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    include_str!("../migrations/0004_revision_bound_canonical_content.sql")
                        .as_bytes()
                )
            ),
        ),
        (
            5,
            "0005_current_intent_effects".to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    include_str!("../migrations/0005_current_intent_effects.sql").as_bytes()
                )
            ),
        ),
        (
            6,
            "0006_publication_authorizations".to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    include_str!("../migrations/0006_publication_authorizations.sql").as_bytes()
                )
            ),
        ),
    ];
    assert_eq!(application_id, 0x4641_4e49);
    assert_eq!(user_version, 6);
    assert_eq!(migrations, expected);
    drop(conn);
    drop(db);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 6);
    let count: i64 = reopened
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 6);
}

#[test]
fn version_one_database_upgrades_transactionally_to_latest() {
    use sha2::{Digest, Sha256};

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fani-v1.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "application_id", 0x4641_4e49_i64)
        .unwrap();
    conn.execute_batch(include_str!("../migrations/0001_native_authority.sql"))
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_migrations(
            version INTEGER PRIMARY KEY CHECK(version > 0),
            name TEXT NOT NULL UNIQUE,
            checksum TEXT NOT NULL CHECK(length(checksum) = 64),
            applied_at INTEGER NOT NULL
        ) STRICT;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO schema_migrations(version,name,checksum,applied_at) VALUES (1,?1,?2,1)",
        params![
            "0001_native_authority",
            format!(
                "{:x}",
                Sha256::digest(include_str!("../migrations/0001_native_authority.sql").as_bytes())
            )
        ],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    drop(conn);

    let upgraded = Database::open(&path).unwrap();
    assert_eq!(upgraded.schema_version().unwrap(), 6);
    let conn = upgraded.connect().unwrap();
    assert!(conn
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version=2 AND name='0002_orthogonal_translation_state'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok());
    assert!(
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='translation_memory_entries'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
    );
}

#[test]
fn orthogonal_translation_state_and_memory_tiers_are_independent() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    let canonical_id = fixture
        .db
        .upsert_canonical_file(CanonicalFileInput {
            repository_id: fixture.repository_id,
            locale: "zh-CN",
            path: "zh-CN/guide.md",
            source_revision: "abc123",
            content: &[1],
            content_hash: "hash",
            materialized_hash: None,
            freshness: Freshness::Exact,
            provenance: TranslationProvenance::Ai,
            validation: ValidationState::Passed,
            review: ReviewState::Approved,
            publication: PublicationState::Candidate,
            trust_tier: MemoryTier::Candidate,
            policy_fingerprint: &fingerprint,
        })
        .unwrap();
    fixture
        .db
        .transition_canonical_file(canonical_id, CanonicalTransition::CommitCreated, None)
        .unwrap();
    fixture
        .db
        .transition_canonical_file(
            canonical_id,
            CanonicalTransition::Materialized,
            Some("hash"),
        )
        .unwrap();
    let conn = fixture.db.connect().unwrap();
    let state: (String, String, String, String, String, String) = conn
        .query_row(
            "SELECT freshness,provenance,validation_state,review_state,publication_state,trust_tier
             FROM canonical_files WHERE path='zh-CN/guide.md'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        state,
        (
            "exact".into(),
            "ai".into(),
            "passed".into(),
            "approved".into(),
            "commit_created".into(),
            "candidate".into(),
        )
    );
    assert!(
        conn.execute(
            "UPDATE canonical_files SET validation_state='not-a-state' WHERE path='zh-CN/guide.md'",
            [],
        )
        .is_err()
    );

    for tier in ["trusted", "candidate", "history"] {
        conn.execute(
            "INSERT INTO translation_memory_entries(
                repository_id,unit_id,locale,source_hash,context_key,target_text,tier,provenance,
                policy_fingerprint,created_at)
             VALUES (?1,?2,'zh-CN','source-hash','Paragraph',?3,?4,'test',?5,1)",
            params![
                fixture.repository_id,
                fixture.unit_id,
                format!("translation-{tier}"),
                tier,
                fingerprint
            ],
        )
        .unwrap();
    }
    let tiers: Vec<String> = {
        let mut statement = conn
            .prepare("SELECT tier FROM translation_memory_entries ORDER BY tier")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(tiers, ["candidate", "history", "trusted"]);
    assert!(conn
        .execute(
            "INSERT INTO translation_memory_entries(
                repository_id,locale,source_hash,target_text,tier,provenance,policy_fingerprint,created_at)
             VALUES (?1,'zh-CN','bad','bad','untrusted','test',?2,1)",
            params![fixture.repository_id, fingerprint],
        )
        .is_err());
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
            provider: "fixture-provider",
            model: "fixture-model",
            adapter: "command-json-v1",
            provider_fingerprint: TEST_FINGERPRINT,
            prompt_version: prompts::PROMPT_VERSION,
            prompt_hash: TEST_FINGERPRINT,
            policy_fingerprint: TEST_FINGERPRINT,
            status: fani::application::contracts::AttemptStatus::Succeeded,
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
            provider: "fixture-provider",
            model: "fixture-model",
            adapter: "command-json-v1",
            provider_fingerprint: TEST_FINGERPRINT,
            prompt_version: prompts::PROMPT_VERSION,
            prompt_hash: TEST_FINGERPRINT,
            policy_fingerprint: TEST_FINGERPRINT,
            status: fani::application::contracts::AttemptStatus::Failed,
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
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: r#"{"prompt":"translate"}"#,
                response_json: Some(response),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:translate:heading:intro",
            target_text: "介绍",
            score: Some(1.0),
            policy_fingerprint: &prompts::policy_fingerprint(),
            provenance: TranslationProvenance::Ai,
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
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: r#"{"prompt":"translate"}"#,
                response_json: Some(response),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:translate:heading:intro",
            target_text: "介绍",
            score: Some(1.0),
            policy_fingerprint: &prompts::policy_fingerprint(),
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();
    assert!(!replay.inserted);
    assert_eq!(replay.id, receipt.id);
}

#[test]
fn candidate_memory_is_not_reused_until_explicitly_trusted() {
    let fixture = fixture();
    fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "candidate-only",
                agent: "translator",
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: r#"{"task":"candidate-only"}"#,
                response_json: Some(r#"{"output":"候选译文"}"#),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:candidate-only",
            target_text: "候选译文",
            score: Some(1.0),
            policy_fingerprint: &prompts::policy_fingerprint(),
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();

    assert_eq!(
        fixture
            .db
            .recoverable_candidate(
                &fixture.run_id,
                fixture.unit_id,
                "zh-CN",
                TEST_FINGERPRINT,
                "fani-leading-strong-separator-v1",
            )
            .unwrap()
            .as_deref(),
        Some("候选译文")
    );
    assert_eq!(
        fixture
            .db
            .recoverable_invocation_candidate(
                "sync:abc123:zh-CN",
                fixture.unit_id,
                "zh-CN",
                TEST_FINGERPRINT,
                "fani-leading-strong-separator-v1",
            )
            .unwrap()
            .as_deref(),
        Some("候选译文")
    );
    let fingerprint = prompts::policy_fingerprint();
    assert_ne!(fingerprint, TEST_FINGERPRINT);
    assert_eq!(
        fixture
            .db
            .recoverable_candidate(
                &fixture.run_id,
                fixture.unit_id,
                "zh-CN",
                &fingerprint,
                "fani-leading-strong-separator-v1",
            )
            .unwrap(),
        None
    );
    let conn = fixture.db.connect().unwrap();
    let persisted: (String, String, String, String) = conn
        .query_row(
            r#"SELECT r.policy_fingerprint,w.policy_fingerprint,
                      tv.policy_fingerprint,tm.policy_fingerprint
               FROM runs r
               JOIN work_items w ON w.run_id=r.id
               JOIN attempts a ON a.work_item_id=w.id
               JOIN translation_versions tv ON tv.source_attempt_id=a.id
               JOIN translation_memory_entries tm ON tm.translation_version_id=tv.id
               WHERE r.id=?1 AND tm.tier='candidate'"#,
            [&fixture.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        persisted,
        (
            fingerprint.clone(),
            fingerprint.clone(),
            fingerprint.clone(),
            fingerprint,
        )
    );
    drop(conn);
    assert_eq!(
        fixture
            .db
            .recoverable_candidate(
                "different-run",
                fixture.unit_id,
                "zh-CN",
                TEST_FINGERPRINT,
                "fani-leading-strong-separator-v1",
            )
            .unwrap(),
        None
    );

    let history = fixture
        .db
        .unit_history(
            fixture
                .db
                .connect()
                .unwrap()
                .query_row(
                    "SELECT document_id FROM units WHERE id=?1",
                    [fixture.unit_id],
                    |row| row.get(0),
                )
                .unwrap(),
            "zh-CN",
        )
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].translation, None);
    assert!(!history[0].trusted);
    assert_eq!(
        fixture
            .db
            .trusted_translation(
                fixture.repository_id,
                "zh-CN",
                "source-hash",
                &fani::domain::markdown::extract_units("Introduction")[0]
                    .memory_context_key("guide.md")
            )
            .unwrap(),
        None
    );

    fixture
        .db
        .trust_translation(TrustTranslationInput {
            repository_id: fixture.repository_id,
            unit_id: Some(fixture.unit_id),
            locale: "zh-CN",
            source_hash: "source-hash",
            context_key: &fani::domain::markdown::extract_units("Introduction")[0]
                .memory_context_key("guide.md"),
            target_text: "人工认可译文",
            provenance: "human_adopted",
            policy_fingerprint: &prompts::policy_fingerprint(),
        })
        .unwrap();
    assert_eq!(
        fixture
            .db
            .trusted_translation(
                fixture.repository_id,
                "zh-CN",
                "source-hash",
                &fani::domain::markdown::extract_units("Introduction")[0]
                    .memory_context_key("guide.md")
            )
            .unwrap()
            .as_deref(),
        Some("人工认可译文")
    );
}

#[test]
fn completed_commit_without_authorization_requires_a_new_checked_effect() {
    let fixture = fixture();
    let key = "publish:orphaned-authorization";
    let id = fixture
        .db
        .enqueue_publication(
            fixture.repository_id,
            Some(&fixture.run_id),
            "zh-CN",
            key,
            r#"{"commit":"historical"}"#,
        )
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    fixture
        .db
        .claim_outbox(OutboxKind::Publication, "publisher", now, 1000)
        .unwrap()
        .unwrap();
    fixture
        .db
        .complete_outbox(OutboxKind::Publication, id, "publisher")
        .unwrap();
    let renewed = fixture.db.effect_key(OutboxKind::Publication, key).unwrap();
    assert_ne!(renewed, key);
    assert_eq!(
        fixture.db.effect_key(OutboxKind::Publication, key).unwrap(),
        renewed
    );
    assert!(
        fixture
            .db
            .publication_snapshot(fixture.repository_id, "zh-CN", "historical")
            .unwrap()
            .is_empty()
    );
    let retained: (String, String) = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT state,payload_json FROM publication_outbox WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        retained,
        ("done".into(), r#"{"commit":"historical"}"#.into())
    );
}

#[test]
fn superseded_publication_key_creates_new_effect_without_reopening_history() {
    let fixture = fixture();
    let base = "publish:roundtrip";
    let old = fixture
        .db
        .enqueue_publication(
            fixture.repository_id,
            Some(&fixture.run_id),
            "zh-CN",
            base,
            "{}",
        )
        .unwrap();
    assert_eq!(
        fixture
            .db
            .effect_key(OutboxKind::Publication, base)
            .unwrap(),
        base
    );
    let now = chrono::Utc::now().timestamp_millis();
    fixture
        .db
        .claim_outbox(OutboxKind::Publication, "publisher", now, 1000)
        .unwrap()
        .unwrap();
    fixture
        .db
        .update_outbox_payload(
            OutboxKind::Publication,
            old,
            "publisher",
            r#"{"superseded_reason":"incompatible"}"#,
        )
        .unwrap();
    fixture
        .db
        .complete_outbox(OutboxKind::Publication, old, "publisher")
        .unwrap();
    let key = fixture
        .db
        .effect_key(OutboxKind::Publication, base)
        .unwrap();
    assert_ne!(key, base);
    let new = fixture
        .db
        .enqueue_publication(
            fixture.repository_id,
            Some(&fixture.run_id),
            "zh-CN",
            &key,
            "{}",
        )
        .unwrap();
    assert_ne!(new, old);
    assert_eq!(
        fixture
            .db
            .effect_key(OutboxKind::Publication, base)
            .unwrap(),
        key
    );
    fixture
        .db
        .claim_outbox(OutboxKind::Publication, "publisher", now + 100, 1000)
        .unwrap()
        .unwrap();
    fixture
        .db
        .complete_outbox(OutboxKind::Publication, new, "publisher")
        .unwrap();
    assert_eq!(
        fixture
            .db
            .effect_key(OutboxKind::Publication, base)
            .unwrap(),
        key
    );
    let retained: (String, String) = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT state,payload_json FROM publication_outbox WHERE id=?1",
            [old],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        retained,
        (
            "done".into(),
            r#"{"superseded_reason":"incompatible"}"#.into()
        )
    );
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
    let now = chrono::Utc::now().timestamp_millis();
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
            .supersede_materializations(
                fixture.repository_id,
                "zh-CN",
                "zh-CN/guide.md",
                active_key
            )
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
        .supersede_materializations(fixture.repository_id, "zh-CN", "zh-CN/guide.md", "newest")
        .unwrap_err();
    assert!(error.to_string().contains("live worker"), "{error:#}");
}

#[test]
fn same_locale_and_path_outboxes_are_repository_scoped() {
    let fixture = fixture();
    let other_repo = fixture
        .db
        .upsert_repository("other", &fixture._temp.path().join("other"), None, None)
        .unwrap();
    let other_document = fixture
        .db
        .upsert_document(other_repo, "guide.md", Some("abc123"), "hash", "{}")
        .unwrap();
    let other_run = fixture
        .db
        .begin_run(
            other_repo,
            "other-run",
            fixture._temp.path().join("config").as_path(),
            "{}",
            TEST_FINGERPRINT,
        )
        .unwrap();
    let other_work = fixture
        .db
        .enqueue_document_work_item(
            &other_run,
            other_document,
            "zh-CN",
            "materialization",
            0,
            "{}",
        )
        .unwrap();
    for (work, key) in [(fixture.work_item_id, "ours"), (other_work, "theirs")] {
        fixture
            .db
            .enqueue_materialization(work, key, r#"{"locale":"zh-CN","path":"same.md"}"#)
            .unwrap();
    }
    assert_eq!(
        fixture
            .db
            .supersede_materializations(fixture.repository_id, "zh-CN", "same.md", "new")
            .unwrap(),
        1
    );
    let conn = fixture.db.connect().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT state FROM materialization_outbox WHERE dedupe_key='theirs'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "pending"
    );
    assert_eq!(
        conn.query_row(
            "SELECT status FROM work_items WHERE id=?1",
            [fixture.work_item_id],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "cancelled"
    );
    let theirs = fixture
        .db
        .enqueue_publication(other_repo, Some(&other_run), "zh-CN", "theirs-pub", "{}")
        .unwrap();
    let ours = fixture
        .db
        .enqueue_publication(
            fixture.repository_id,
            Some(&fixture.run_id),
            "zh-CN",
            "ours-pub",
            "{}",
        )
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis() + 1000;
    assert_eq!(
        fixture
            .db
            .claim_publication_locale(fixture.repository_id, "zh-CN", "ours-owner", now, 10000)
            .unwrap()
            .unwrap()
            .id,
        ours
    );
    assert_eq!(
        fixture
            .db
            .claim_publication_locale(other_repo, "zh-CN", "other-owner", now, 10000)
            .unwrap()
            .unwrap()
            .id,
        theirs
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
fn merged_publication_promotes_only_exact_manifest_translation_versions() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "published-candidate",
                agent: "translator",
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: r#"{"task":"published"}"#,
                response_json: Some(r#"{"output":"已发布译文"}"#),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:published",
            target_text: "已发布译文",
            score: Some(1.0),
            policy_fingerprint: &fingerprint,
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();

    let other_document_id = fixture
        .db
        .upsert_document(
            fixture.repository_id,
            "other.md",
            Some("abc123"),
            "other-doc-hash",
            "{}",
        )
        .unwrap();
    let other_unit_id = fixture
        .db
        .upsert_unit(
            other_document_id,
            "paragraph:other",
            0,
            "Other",
            "other-source-hash",
            &fani::domain::document::unit_metadata(
                &fani::domain::markdown::extract_units("Other")[0],
                "other.md",
            ),
        )
        .unwrap();
    let other_work_item_id = fixture
        .db
        .enqueue_work_item(
            &fixture.run_id,
            other_unit_id,
            "zh-CN",
            "translate",
            10,
            "{}",
        )
        .unwrap();
    fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: other_work_item_id,
                dedupe_key: "unpublished-candidate",
                agent: "translator",
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: r#"{"task":"unpublished"}"#,
                response_json: Some(r#"{"output":"未发布译文"}"#),
                error: None,
            },
            unit_id: other_unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:unpublished",
            target_text: "未发布译文",
            score: Some(1.0),
            policy_fingerprint: &fingerprint,
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();

    let canonical_id = fixture
        .db
        .upsert_canonical_file(CanonicalFileInput {
            repository_id: fixture.repository_id,
            locale: "zh-CN",
            path: "zh-CN/guide.md",
            source_revision: "abc123",
            content: "# 已发布译文\n".as_bytes(),
            content_hash: "published-content-hash",
            materialized_hash: Some("published-content-hash"),
            freshness: Freshness::Exact,
            provenance: TranslationProvenance::Ai,
            validation: ValidationState::Passed,
            review: ReviewState::Unreviewed,
            publication: PublicationState::Candidate,
            trust_tier: MemoryTier::Candidate,
            policy_fingerprint: &fingerprint,
        })
        .unwrap();
    assert_eq!(
        fixture
            .db
            .record_canonical_file_translations(
                canonical_id,
                &[CanonicalTranslationInput {
                    unit_id: fixture.unit_id,
                    target_text: "已发布译文",
                }],
                "zh-CN",
            )
            .unwrap(),
        1
    );
    let canonical_content_version_id: i64 = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT current_content_version_id FROM canonical_files WHERE id=?1",
            [canonical_id],
            |row| row.get(0),
        )
        .unwrap();
    fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "candidate-commit",
            policy_fingerprint: &fingerprint,
            files: &[PublicationManifestFile {
                canonical_content_version_id,
                canonical_file_id: canonical_id,
                content_hash: "published-content-hash".into(),
            }],
        })
        .unwrap();
    assert_eq!(
        fixture
            .db
            .promote_merged_publication(
                fixture.repository_id,
                "zh-CN",
                "candidate-commit",
                "github_merged",
            )
            .unwrap(),
        1
    );

    let conn = fixture.db.connect().unwrap();
    let trusted = {
        let mut statement = conn
            .prepare(
                "SELECT source_hash,target_text FROM translation_memory_entries WHERE tier='trusted' AND superseded_at IS NULL ORDER BY source_hash",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(trusted, vec![("source-hash".into(), "已发布译文".into())]);
    let states = {
        let mut statement = conn
            .prepare(
                "SELECT tm.source_hash,tv.publication_state FROM translation_memory_entries tm JOIN translation_versions tv ON tv.id=tm.translation_version_id WHERE tm.tier='candidate' ORDER BY tm.source_hash",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    assert_eq!(
        states,
        vec![
            ("other-source-hash".into(), "candidate".into()),
            ("source-hash".into(), "merged".into()),
        ]
    );
    let manifest_state: String = conn
        .query_row(
            "SELECT state FROM publication_manifests WHERE candidate_commit='candidate-commit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(manifest_state, "merged");
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
            provider: "fixture-provider",
            model: "fixture-model",
            adapter: "command-json-v1",
            provider_fingerprint: TEST_FINGERPRINT,
            prompt_version: prompts::PROMPT_VERSION,
            prompt_hash: TEST_FINGERPRINT,
            policy_fingerprint: TEST_FINGERPRINT,
            status: fani::application::contracts::AttemptStatus::Succeeded,
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
            context_key: &fani::domain::markdown::extract_units("Introduction")[0]
                .memory_context_key("guide.md"),
            target_text: "介绍",
            provenance: "reviewed",
            policy_fingerprint: &prompts::policy_fingerprint(),
        })
        .unwrap();
    let replay_tm = fixture
        .db
        .trust_translation(TrustTranslationInput {
            repository_id: fixture.repository_id,
            unit_id: Some(fixture.unit_id),
            locale: "zh-CN",
            source_hash: "source-hash",
            context_key: &fani::domain::markdown::extract_units("Introduction")[0]
                .memory_context_key("guide.md"),
            target_text: "简介",
            provenance: "approved",
            policy_fingerprint: &prompts::policy_fingerprint(),
        })
        .unwrap();

    assert_ne!(first_tm, replay_tm);
    let old_tier: String = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT tier FROM translation_memory_entries WHERE id=?1 AND superseded_at IS NOT NULL",
            [first_tm],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_tier, "history");
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
            "SELECT COUNT(*),target_text,json_extract(provenance,'$.origin') FROM translation_memory_entries WHERE repository_id=?1 AND locale='zh-CN' AND tier='trusted' AND superseded_at IS NULL",
            [fixture.repository_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(tm, (1, "简介".into(), "approved".into()));
    fixture.db.integrity_check().unwrap();
}

fn canonical_version(fixture: &Fixture, path: &str, content_hash: &str) -> (i64, i64) {
    let canonical = fixture
        .db
        .persist_canonical_file(
            CanonicalFileInput {
                repository_id: fixture.repository_id,
                locale: "zh-CN",
                path,
                source_revision: "abc123",
                content: content_hash.as_bytes(),
                content_hash,
                materialized_hash: Some(content_hash),
                freshness: Freshness::Exact,
                provenance: TranslationProvenance::Ai,
                validation: ValidationState::Passed,
                review: ReviewState::Unreviewed,
                publication: PublicationState::Candidate,
                trust_tier: MemoryTier::Candidate,
                policy_fingerprint: &prompts::policy_fingerprint(),
            },
            &[],
        )
        .unwrap();
    (canonical.id, canonical.content_version_id)
}

#[test]
fn canonical_content_links_trusted_assembled_text_not_stale_selected_candidate() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "stale-selected",
                agent: "translator",
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: "{}",
                response_json: Some(r#"{"output":"陈旧候选"}"#),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:stale",
            target_text: "陈旧候选",
            score: Some(1.0),
            policy_fingerprint: &fingerprint,
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();
    fixture
        .db
        .trust_translation(TrustTranslationInput {
            repository_id: fixture.repository_id,
            unit_id: Some(fixture.unit_id),
            locale: "zh-CN",
            source_hash: "source-hash",
            context_key: "Paragraph",
            target_text: "可信组装译文",
            provenance: "reviewed",
            policy_fingerprint: &fingerprint,
        })
        .unwrap();

    let canonical = fixture
        .db
        .persist_canonical_file(
            CanonicalFileInput {
                repository_id: fixture.repository_id,
                locale: "zh-CN",
                path: "zh-CN/trusted.md",
                source_revision: "abc123",
                content: "# 可信组装译文\n".as_bytes(),
                content_hash: "trusted-assembled-hash",
                materialized_hash: Some("trusted-assembled-hash"),
                freshness: Freshness::Exact,
                provenance: TranslationProvenance::Ai,
                validation: ValidationState::Passed,
                review: ReviewState::Unreviewed,
                publication: PublicationState::Candidate,
                trust_tier: MemoryTier::Candidate,
                policy_fingerprint: &fingerprint,
            },
            &[CanonicalTranslationInput {
                unit_id: fixture.unit_id,
                target_text: "可信组装译文",
            }],
        )
        .unwrap();

    let conn = fixture.db.connect().unwrap();
    let linked: String = conn
        .query_row(
            r#"SELECT tv.target_text
               FROM canonical_file_translations cft
               JOIN translation_versions tv ON tv.id=cft.translation_version_id
               WHERE cft.canonical_content_version_id=?1"#,
            [canonical.content_version_id],
            |row| row.get(0),
        )
        .unwrap();
    let selected: String = conn
        .query_row(
            "SELECT target_text FROM canonical_candidates WHERE unit_id=?1 AND locale='zh-CN' AND selected=1",
            [fixture.unit_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(linked, "可信组装译文");
    assert_eq!(selected, "陈旧候选");
}

#[test]
fn merged_tm_uses_immutable_source_version_after_live_source_mutation() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "immutable-source",
                agent: "translator",
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: "{}",
                response_json: Some(r#"{"output":"不可变来源译文"}"#),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:immutable",
            target_text: "不可变来源译文",
            score: Some(1.0),
            policy_fingerprint: &fingerprint,
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();
    let canonical = fixture
        .db
        .persist_canonical_file(
            CanonicalFileInput {
                repository_id: fixture.repository_id,
                locale: "zh-CN",
                path: "zh-CN/immutable.md",
                source_revision: "abc123",
                content: "# 不可变来源译文\n".as_bytes(),
                content_hash: "immutable-content-hash",
                materialized_hash: Some("immutable-content-hash"),
                freshness: Freshness::Exact,
                provenance: TranslationProvenance::Ai,
                validation: ValidationState::Passed,
                review: ReviewState::Unreviewed,
                publication: PublicationState::Candidate,
                trust_tier: MemoryTier::Candidate,
                policy_fingerprint: &fingerprint,
            },
            &[CanonicalTranslationInput {
                unit_id: fixture.unit_id,
                target_text: "不可变来源译文",
            }],
        )
        .unwrap();
    fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "immutable-commit",
            policy_fingerprint: &fingerprint,
            files: &[PublicationManifestFile {
                canonical_content_version_id: canonical.content_version_id,
                canonical_file_id: canonical.id,
                content_hash: canonical.content_hash.clone(),
            }],
        })
        .unwrap();

    let document_id: i64 = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT document_id FROM units WHERE id=?1",
            [fixture.unit_id],
            |row| row.get(0),
        )
        .unwrap();
    fixture
        .db
        .upsert_document(
            fixture.repository_id,
            "guide.md",
            Some("def456"),
            "mutated-document-hash",
            "{}",
        )
        .unwrap();
    fixture
        .db
        .upsert_unit(
            document_id,
            "heading:intro",
            0,
            "Mutated introduction",
            "mutated-source-hash",
            r#"{"kind":"Paragraph"}"#,
        )
        .unwrap();
    fixture
        .db
        .promote_merged_publication(
            fixture.repository_id,
            "zh-CN",
            "immutable-commit",
            "github_merged",
        )
        .unwrap();

    let tm: (String, String, String) = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            "SELECT source_hash,source_revision,context_key FROM translation_memory_entries WHERE tier='trusted' AND target_text='不可变来源译文'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        tm,
        (
            "source-hash".into(),
            "abc123".into(),
            fani::domain::markdown::extract_units("Introduction")[0].memory_context_key("guide.md")
        )
    );
}

#[test]
fn publication_authorizations_bind_run_effect_and_exact_commit_snapshot() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    let (file, version) = canonical_version(&fixture, "zh-CN/first.md", "first-hash");
    let (other_file, other_version) = canonical_version(&fixture, "zh-CN/other.md", "other-hash");
    let files = [PublicationManifestFile {
        canonical_content_version_id: version,
        canonical_file_id: file,
        content_hash: "first-hash".into(),
    }];
    let other_files = [PublicationManifestFile {
        canonical_content_version_id: other_version,
        canonical_file_id: other_file,
        content_hash: "other-hash".into(),
    }];
    let second_run = fixture
        .db
        .begin_run(
            fixture.repository_id,
            "second-publication-run",
            std::path::Path::new("fani.toml"),
            "{}",
            &fingerprint,
        )
        .unwrap();
    let input = |run, commit, files| PublicationManifestInput {
        repository_id: fixture.repository_id,
        run_id: run,
        locale: "zh-CN",
        source_revision: "abc123",
        candidate_commit: commit,
        policy_fingerprint: &fingerprint,
        files,
    };
    let first = fixture
        .db
        .record_publication_authorization(input(&fixture.run_id, "shared", &files), "effect-a")
        .unwrap();
    let second = fixture
        .db
        .record_publication_authorization(input(&second_run, "shared", &files), "effect-b")
        .unwrap();
    assert_ne!(first, second);
    assert!(
        fixture
            .db
            .transition_publication_authorization(
                fixture.repository_id,
                "zh-CN",
                "shared",
                Some("missing-effect"),
                PublicationState::PushPending
            )
            .is_err()
    );
    fixture
        .db
        .transition_publication_authorization(
            fixture.repository_id,
            "zh-CN",
            "shared",
            Some("missing-effect"),
            PublicationState::Superseded,
        )
        .unwrap();
    assert!(
        fixture
            .db
            .record_publication_authorization(
                input(&second_run, "shared", &other_files),
                "effect-c"
            )
            .is_err()
    );
    assert!(
        fixture
            .db
            .record_publication_authorization(input(&fixture.run_id, "shared", &files), "effect-b")
            .is_err()
    );
    assert!(
        fixture
            .db
            .record_publication_authorization(
                input(&second_run, "different-commit", &files),
                "effect-b"
            )
            .is_err()
    );
    fixture
        .db
        .transition_publication_authorization(
            fixture.repository_id,
            "zh-CN",
            "shared",
            Some("effect-b"),
            PublicationState::Superseded,
        )
        .unwrap();
    assert_eq!(
        fixture
            .db
            .record_publication_authorization(input(&second_run, "shared", &files), "effect-b")
            .unwrap(),
        second
    );
    let conn = fixture.db.connect().unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM publication_manifests", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM publication_manifests WHERE id=?1",
            [first],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "commit_created"
    );
    assert_eq!(
        conn.query_row(
            "SELECT state FROM publication_manifests WHERE id=?1",
            [second],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "superseded"
    );
    assert_eq!(
        fixture
            .db
            .publication_snapshot(fixture.repository_id, "zh-CN", "shared")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn publication_manifest_rejects_swapped_multi_file_hashes() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    let (first_file, first_version) = canonical_version(&fixture, "zh-CN/first.md", "first-hash");
    let (second_file, second_version) =
        canonical_version(&fixture, "zh-CN/second.md", "second-hash");
    let error = fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "swapped-commit",
            policy_fingerprint: &fingerprint,
            files: &[
                PublicationManifestFile {
                    canonical_content_version_id: first_version,
                    canonical_file_id: first_file,
                    content_hash: "second-hash".into(),
                },
                PublicationManifestFile {
                    canonical_content_version_id: second_version,
                    canonical_file_id: second_file,
                    content_hash: "first-hash".into(),
                },
            ],
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not match publication manifest content")
    );
}

#[test]
fn merged_publication_remains_terminal_under_manifest_replay() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    fixture
        .db
        .record_attempt_candidate(AttemptCandidateInput {
            attempt: AttemptInput {
                work_item_id: fixture.work_item_id,
                dedupe_key: "merged-replay",
                agent: "translator",
                provider: "fixture-provider",
                model: "fixture-model",
                adapter: "command-json-v1",
                provider_fingerprint: TEST_FINGERPRINT,
                prompt_version: prompts::PROMPT_VERSION,
                prompt_hash: TEST_FINGERPRINT,
                policy_fingerprint: TEST_FINGERPRINT,
                status: fani::application::contracts::AttemptStatus::Succeeded,
                request_json: "{}",
                response_json: Some(r#"{"output":"终态译文"}"#),
                error: None,
            },
            unit_id: fixture.unit_id,
            locale: "zh-CN",
            candidate_key: "attempt:merged-replay",
            target_text: "终态译文",
            score: Some(1.0),
            policy_fingerprint: &fingerprint,
            provenance: TranslationProvenance::Ai,
        })
        .unwrap();
    let canonical = fixture
        .db
        .persist_canonical_file(
            CanonicalFileInput {
                repository_id: fixture.repository_id,
                locale: "zh-CN",
                path: "zh-CN/terminal.md",
                source_revision: "abc123",
                content: "终态译文".as_bytes(),
                content_hash: "terminal-hash",
                materialized_hash: Some("terminal-hash"),
                freshness: Freshness::Exact,
                provenance: TranslationProvenance::Ai,
                validation: ValidationState::Passed,
                review: ReviewState::Unreviewed,
                publication: PublicationState::Candidate,
                trust_tier: MemoryTier::Candidate,
                policy_fingerprint: &fingerprint,
            },
            &[CanonicalTranslationInput {
                unit_id: fixture.unit_id,
                target_text: "终态译文",
            }],
        )
        .unwrap();
    let files = [PublicationManifestFile {
        canonical_content_version_id: canonical.content_version_id,
        canonical_file_id: canonical.id,
        content_hash: canonical.content_hash.clone(),
    }];
    fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "terminal-commit",
            policy_fingerprint: &fingerprint,
            files: &files,
        })
        .unwrap();
    fixture
        .db
        .promote_merged_publication(
            fixture.repository_id,
            "zh-CN",
            "terminal-commit",
            "github_merged",
        )
        .unwrap();
    fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "terminal-commit",
            policy_fingerprint: &fingerprint,
            files: &files,
        })
        .unwrap();
    fixture
        .db
        .transition_publication_manifest(
            fixture.repository_id,
            "zh-CN",
            "terminal-commit",
            PublicationState::PushPending,
        )
        .unwrap();

    let states: (String, String, String, String) = fixture
        .db
        .connect()
        .unwrap()
        .query_row(
            r#"SELECT pm.state,ccv.publication_state,cf.publication_state,tv.publication_state
               FROM publication_manifests pm
               JOIN publication_manifest_files pmf ON pmf.manifest_id=pm.id
               JOIN canonical_content_versions ccv ON ccv.id=pmf.canonical_content_version_id
               JOIN canonical_files cf ON cf.current_content_version_id=ccv.id
               JOIN canonical_file_translations cft ON cft.canonical_content_version_id=ccv.id
               JOIN translation_versions tv ON tv.id=cft.translation_version_id
               WHERE pm.candidate_commit='terminal-commit'"#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        states,
        (
            "merged".into(),
            "merged".into(),
            "merged".into(),
            "merged".into()
        )
    );
}

#[test]
fn publication_manifest_replay_requires_the_exact_file_set() {
    let fixture = fixture();
    let fingerprint = prompts::policy_fingerprint();
    let (first_file, first_version) = canonical_version(&fixture, "zh-CN/replay-a.md", "replay-a");
    let (second_file, second_version) =
        canonical_version(&fixture, "zh-CN/replay-b.md", "replay-b");
    let (third_file, third_version) = canonical_version(&fixture, "zh-CN/replay-c.md", "replay-c");
    fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "replay-set-commit",
            policy_fingerprint: &fingerprint,
            files: &[
                PublicationManifestFile {
                    canonical_content_version_id: first_version,
                    canonical_file_id: first_file,
                    content_hash: "replay-a".into(),
                },
                PublicationManifestFile {
                    canonical_content_version_id: second_version,
                    canonical_file_id: second_file,
                    content_hash: "replay-b".into(),
                },
            ],
        })
        .unwrap();
    let error = fixture
        .db
        .record_publication_manifest(PublicationManifestInput {
            repository_id: fixture.repository_id,
            run_id: &fixture.run_id,
            locale: "zh-CN",
            source_revision: "abc123",
            candidate_commit: "replay-set-commit",
            policy_fingerprint: &fingerprint,
            files: &[
                PublicationManifestFile {
                    canonical_content_version_id: first_version,
                    canonical_file_id: first_file,
                    content_hash: "replay-a".into(),
                },
                PublicationManifestFile {
                    canonical_content_version_id: third_version,
                    canonical_file_id: third_file,
                    content_hash: "replay-c".into(),
                },
            ],
        })
        .unwrap_err();
    assert!(error.to_string().contains("file set conflicts"));
}
