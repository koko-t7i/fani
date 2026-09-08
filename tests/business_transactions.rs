use fani::application::{contracts::*, ports::*};
use fani::domain::{
    document::{DocumentFormat, format_contract},
    model::*,
    prompts,
};
use fani::test_support::db::Database;
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    db: Database,
    repository: i64,
    document: i64,
    run: String,
    policy: String,
    identity: DocumentIdentity,
}
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(temp.path().join("fani.db")).unwrap();
    let repository = db
        .upsert_repository("repo", temp.path(), None, None)
        .unwrap();
    let document = db
        .upsert_document(repository, "guide.md", Some("revision"), "source", "{}")
        .unwrap();
    let policy = prompts::policy_fingerprint();
    let run = db
        .begin_run(
            repository,
            "run",
            &temp.path().join("fani.toml"),
            "{}",
            &policy,
        )
        .unwrap();
    let identity = DocumentIdentity {
        source_path: "guide.md".into(),
        source_revision: "revision".into(),
        source_hash: "source".into(),
        source_set_id: "set".into(),
        mapping_identity: "mapping".into(),
        locale: "fr".into(),
        target_path: "fr/guide.md".into(),
        contract: format_contract(DocumentFormat::Markdown).unwrap(),
        request_schema: "fani.agent.request.v2".into(),
        policy_fingerprint: policy.clone(),
        request_identity: "identity".into(),
    };
    Fixture {
        _temp: temp,
        db,
        repository,
        document,
        run,
        policy,
        identity,
    }
}
fn canonical<'a>(f: &'a Fixture, content: &'a str) -> CanonicalFileInput<'a> {
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
        policy_fingerprint: &f.policy,
    }
}
fn materialize(f: &Fixture, content: &str) -> anyhow::Result<CanonicalMaterializationReceipt> {
    f.db.commit_materialization(CanonicalMaterializationInput {
        canonical: canonical(f, content),
        translations: &[],
        identity: &f.identity,
        run_id: &f.run,
        document_id: f.document,
        base_dedupe_key: &format!("materialize:{content}"),
        observed_target_matches: false,
    })
}
fn count(f: &Fixture, table: &str) -> i64 {
    f.db.connect()
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
fn trigger(f: &Fixture, sql: &str) {
    f.db.connect().unwrap().execute_batch(sql).unwrap();
}

#[test]
fn failed_materialization_intent_rolls_back_canonical_identity_and_effect_allocation() {
    let f = fixture();
    let old = materialize(&f, "old").unwrap();
    trigger(
        &f,
        "CREATE TRIGGER fail_intent BEFORE INSERT ON materialization_outbox WHEN NEW.dedupe_key='materialize:new' BEGIN SELECT RAISE(ABORT,'injected intent failure'); END;",
    );
    assert!(
        materialize(&f, "new")
            .err()
            .unwrap()
            .to_string()
            .contains("injected intent failure")
    );
    let current =
        f.db.canonical_file(f.repository, "fr", "fr/guide.md")
            .unwrap()
            .unwrap();
    assert_eq!(current.content_version_id, old.canonical.content_version_id);
    for table in [
        "canonical_content_versions",
        "canonical_document_intents",
        "work_items",
        "materialization_outbox",
    ] {
        assert_eq!(count(&f, table), 1, "{table}");
    }
    let state: String =
        f.db.connect()
            .unwrap()
            .query_row("SELECT state FROM materialization_outbox", [], |r| r.get(0))
            .unwrap();
    assert_eq!(state, "pending");
}

#[test]
fn materialization_ack_failure_rolls_back_canonical_and_work_completion() {
    let f = fixture();
    let prepared = materialize(&f, "content").unwrap();
    let receipt = prepared.materialization.unwrap();
    f.db.claim_outbox_key(
        OutboxKind::Materialization,
        &prepared.dedupe_key,
        "owner",
        chrono::Utc::now().timestamp_millis() + 1,
        60000,
    )
    .unwrap()
    .unwrap();
    trigger(
        &f,
        "CREATE TRIGGER fail_ack BEFORE UPDATE OF state ON materialization_outbox WHEN NEW.state='done' BEGIN SELECT RAISE(ABORT,'injected ack failure'); END;",
    );
    let result = MaterializationWorkResult {
        status: MaterializationStatus::Written,
        content_hash: "content".into(),
        code: None,
    };
    assert!(
        f.db.settle_materialization(MaterializationSettlementInput {
            outbox_id: receipt.outbox_id,
            owner: "owner",
            canonical_file_id: prepared.canonical.id,
            work_item_id: receipt.work_item_id,
            transition: Some(CanonicalTransition::Materialized),
            materialized_hash: Some("content"),
            succeeded: true,
            result: &result
        })
        .unwrap_err()
        .to_string()
        .contains("injected ack failure")
    );
    let state:(String,String,String)=f.db.connect().unwrap().query_row("SELECT c.state,w.status,o.state FROM canonical_files c,work_items w,materialization_outbox o",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        state,
        ("candidate".into(), "pending".into(), "processing".into())
    );
}

#[test]
fn preparation_failure_rolls_back_document_units_and_pipeline_work() {
    let f = fixture();
    trigger(
        &f,
        "CREATE TRIGGER fail_pipeline BEFORE INSERT ON work_items BEGIN SELECT RAISE(ABORT,'injected pipeline failure'); END;",
    );
    let units = [PreparedUnitInput {
        unit_key: "heading".into(),
        ordinal: 0,
        source_text: "Title".into(),
        source_hash: "title-hash".into(),
        context_json: "{}".into(),
        context_key: "heading".into(),
        candidate: None,
        enqueue: Some(PipelineRequest {
            source_revision: "revision".into(),
            path: "guide.md".into(),
            unit: "heading".into(),
            document_identity: f.identity.clone(),
        }),
    }];
    let result = f.db.prepare_document(PreparedDocumentInput {
        repository_id: f.repository,
        run_id: &f.run,
        locale: "fr",
        path: "guide.md",
        source_revision: "revision",
        content_hash: "updated",
        metadata_json: "{}",
        units: &units,
    });
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("injected pipeline failure")
    );
    assert_eq!(count(&f, "units"), 0);
    assert_eq!(count(&f, "unit_versions"), 0);
    assert_eq!(count(&f, "work_items"), 0);
    let hash: String =
        f.db.connect()
            .unwrap()
            .query_row("SELECT content_hash FROM documents", [], |r| r.get(0))
            .unwrap();
    assert_eq!(hash, "source");
}

fn publication_payload(f: &Fixture, c: &CanonicalFile) -> PublicationPayload {
    PublicationPayload {
        superseded_reason: None,
        commit: Some("commit".into()),
        expected_remote_tip: Some(None),
        files: vec![PublicationRecord {
            document_identity: Some(f.identity.clone()),
            canonical_content_version_id: c.content_version_id,
            canonical_file_id: c.id,
            content: "content".into(),
            content_hash: "content".into(),
            path: "fr/guide.md".into(),
        }],
        language: "fr".into(),
        policy_fingerprint: f.policy.clone(),
        run_id: f.run.clone(),
        source_revision: "revision".into(),
    }
}
#[test]
fn publication_candidate_failure_rolls_back_authorization_and_payload() {
    let f = fixture();
    let prepared = materialize(&f, "content").unwrap();
    let payload = publication_payload(&f, &prepared.canonical);
    let mut initial = payload.clone();
    initial.commit = None;
    let key =
        f.db.schedule_publication(PublicationIntentInput {
            repository_id: f.repository,
            run_id: &f.run,
            locale: "fr",
            base_dedupe_key: "publication",
            payload: &initial,
        })
        .unwrap();
    let entry =
        f.db.claim_publication_locale(
            f.repository,
            "fr",
            "owner",
            chrono::Utc::now().timestamp_millis() + 1,
            60000,
        )
        .unwrap()
        .unwrap();
    trigger(
        &f,
        "CREATE TRIGGER fail_candidate BEFORE UPDATE OF payload_json ON publication_outbox BEGIN SELECT RAISE(ABORT,'injected candidate failure'); END;",
    );
    let files = [PublicationManifestFile {
        canonical_content_version_id: prepared.canonical.content_version_id,
        canonical_file_id: prepared.canonical.id,
        content_hash: "content".into(),
    }];
    assert!(
        f.db.persist_publication_candidate(PublicationCandidateInput {
            manifest: PublicationManifestInput {
                repository_id: f.repository,
                run_id: &f.run,
                locale: "fr",
                source_revision: "revision",
                candidate_commit: "commit",
                policy_fingerprint: &f.policy,
                files: &files
            },
            authorization_key: &key,
            outbox_id: entry.id,
            owner: "owner",
            payload: &payload,
            state: PublicationState::PushPending
        })
        .unwrap_err()
        .to_string()
        .contains("injected candidate failure")
    );
    assert_eq!(count(&f, "publication_manifests"), 0);
    assert_eq!(count(&f, "publication_manifest_files"), 0);
    let commit: Option<String> =
        f.db.connect()
            .unwrap()
            .query_row(
                "SELECT json_extract(payload_json,'$.commit') FROM publication_outbox",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(commit, None);
}

#[test]
fn publication_settlement_failure_rolls_back_pr_observation_and_authorization() {
    let f = fixture();
    let prepared = materialize(&f, "content").unwrap();
    let payload = publication_payload(&f, &prepared.canonical);
    let key =
        f.db.schedule_publication(PublicationIntentInput {
            repository_id: f.repository,
            run_id: &f.run,
            locale: "fr",
            base_dedupe_key: "publication",
            payload: &payload,
        })
        .unwrap();
    let entry =
        f.db.claim_publication_locale(
            f.repository,
            "fr",
            "owner",
            chrono::Utc::now().timestamp_millis() + 1,
            60000,
        )
        .unwrap()
        .unwrap();
    let files = [PublicationManifestFile {
        canonical_content_version_id: prepared.canonical.content_version_id,
        canonical_file_id: prepared.canonical.id,
        content_hash: "content".into(),
    }];
    f.db.persist_publication_candidate(PublicationCandidateInput {
        manifest: PublicationManifestInput {
            repository_id: f.repository,
            run_id: &f.run,
            locale: "fr",
            source_revision: "revision",
            candidate_commit: "commit",
            policy_fingerprint: &f.policy,
            files: &files,
        },
        authorization_key: &key,
        outbox_id: entry.id,
        owner: "owner",
        payload: &payload,
        state: PublicationState::PushPending,
    })
    .unwrap();
    trigger(
        &f,
        "CREATE TRIGGER fail_publication_ack BEFORE UPDATE OF state ON publication_outbox WHEN NEW.state='done' BEGIN SELECT RAISE(ABORT,'injected publication ack failure'); END;",
    );
    assert!(
        f.db.settle_publication(PublicationSettlementInput {
            repository_id: f.repository,
            locale: "fr",
            candidate_commit: Some("commit"),
            authorization_key: &key,
            outbox_id: entry.id,
            owner: "owner",
            payload: None,
            state: Some(PublicationState::PrOpen),
            pull_request: Some(PullRequestStateInput {
                repository_id: f.repository,
                provider: "github",
                external_id: "1",
                number: Some(1),
                branch: "fr",
                url: None,
                state: "open",
                head_revision: Some("commit"),
                event_key: "open",
                payload_json: "{}"
            }),
            promotion: None
        })
        .unwrap_err()
        .to_string()
        .contains("injected publication ack failure")
    );
    assert_eq!(count(&f, "pull_requests"), 0);
    assert_eq!(count(&f, "pr_events"), 0);
    let state: String =
        f.db.connect()
            .unwrap()
            .query_row("SELECT state FROM publication_manifests", [], |r| r.get(0))
            .unwrap();
    assert_eq!(state, "push_pending");
}

#[test]
fn adoption_work_failure_rolls_back_new_canonical_and_source_registration() {
    let f = fixture();
    trigger(
        &f,
        "CREATE TRIGGER fail_adoption BEFORE UPDATE OF status ON work_items WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected adoption failure'); END;",
    );
    let mut input = canonical(&f, "human");
    input.trust_tier = MemoryTier::Trusted;
    input.review = ReviewState::Approved;
    assert!(
        f.db.adopt_document(AdoptDocumentInput {
            canonical: input,
            run_id: &f.run,
            identity: &f.identity,
            units: &[],
            metadata: &SourceDocumentMetadata {
                format: DocumentFormat::Markdown,
                contract: f.identity.contract.clone()
            }
        })
        .unwrap_err()
        .to_string()
        .contains("injected adoption failure")
    );
    for table in [
        "canonical_files",
        "canonical_content_versions",
        "canonical_document_intents",
        "work_items",
    ] {
        assert_eq!(count(&f, table), 0, "{table}");
    }
}

#[test]
fn discard_recovery_preserves_observed_hash_and_atomic_completion() {
    let f = fixture();
    let prepared = materialize(&f, "content").unwrap();
    let begin = |hash| {
        f.db.begin_discard(DiscardIntentInput {
            repository_id: f.repository,
            run_id: &f.run,
            document_id: f.document,
            locale: "fr",
            path: "fr/guide.md",
            canonical_file_id: prepared.canonical.id,
            canonical_content_version_id: prepared.canonical.content_version_id,
            content_hash: "content",
            observed_hash: Some(hash),
        })
        .unwrap()
    };
    let first = begin("human-v1");
    let resumed = begin("human-v2");
    assert_eq!(first.materialization, resumed.materialization);
    assert_eq!(resumed.expected_hash.as_deref(), Some("human-v1"));
    trigger(
        &f,
        "CREATE TRIGGER fail_discard BEFORE UPDATE OF state ON materialization_outbox WHEN NEW.state='done' AND json_extract(NEW.payload_json,'$.operation')='discard' BEGIN SELECT RAISE(ABORT,'injected discard failure'); END;",
    );
    assert!(
        f.db.complete_discard(&resumed.materialization, prepared.canonical.id, "content")
            .unwrap_err()
            .to_string()
            .contains("injected discard failure")
    );
    let status: String =
        f.db.connect()
            .unwrap()
            .query_row(
                "SELECT status FROM work_items WHERE id=?1",
                [resumed.materialization.work_item_id],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(
        f.db.canonical_file(f.repository, "fr", "fr/guide.md")
            .unwrap()
            .unwrap()
            .state,
        "candidate"
    );
}

#[test]
fn stale_materialization_receipt_cannot_complete_different_canonical_bytes() {
    let f = fixture();
    let prepared = materialize(&f, "first").unwrap();
    let receipt = prepared.materialization.unwrap();
    f.db.claim_outbox_key(
        OutboxKind::Materialization,
        &prepared.dedupe_key,
        "owner",
        chrono::Utc::now().timestamp_millis() + 1,
        60000,
    )
    .unwrap()
    .unwrap();
    f.db.persist_canonical_document(canonical(&f, "second"), &[], &encode(&f.identity).unwrap())
        .unwrap();
    let result = MaterializationWorkResult {
        status: MaterializationStatus::Written,
        content_hash: "first".into(),
        code: None,
    };
    let error =
        f.db.settle_materialization(MaterializationSettlementInput {
            outbox_id: receipt.outbox_id,
            owner: "owner",
            canonical_file_id: prepared.canonical.id,
            work_item_id: receipt.work_item_id,
            transition: Some(CanonicalTransition::Materialized),
            materialized_hash: Some("first"),
            succeeded: true,
            result: &result,
        })
        .unwrap_err();
    assert!(error.to_string().contains("current canonical bytes"));
    let state: String =
        f.db.connect()
            .unwrap()
            .query_row("SELECT state FROM materialization_outbox", [], |r| r.get(0))
            .unwrap();
    assert_eq!(state, "processing");
    assert_eq!(
        f.db.canonical_file(f.repository, "fr", "fr/guide.md")
            .unwrap()
            .unwrap()
            .content,
        b"second"
    );
}

#[test]
fn payload_file_substitution_cannot_create_a_publication_authorization() {
    let f = fixture();
    let prepared = materialize(&f, "content").unwrap();
    let mut payload = publication_payload(&f, &prepared.canonical);
    let key =
        f.db.schedule_publication(PublicationIntentInput {
            repository_id: f.repository,
            run_id: &f.run,
            locale: "fr",
            base_dedupe_key: "publication",
            payload: &payload,
        })
        .unwrap();
    let entry =
        f.db.claim_publication_locale(
            f.repository,
            "fr",
            "owner",
            chrono::Utc::now().timestamp_millis() + 1,
            60000,
        )
        .unwrap()
        .unwrap();
    payload.files[0].content = "substituted".into();
    let files = [PublicationManifestFile {
        canonical_content_version_id: prepared.canonical.content_version_id,
        canonical_file_id: prepared.canonical.id,
        content_hash: "content".into(),
    }];
    let error =
        f.db.persist_publication_candidate(PublicationCandidateInput {
            manifest: PublicationManifestInput {
                repository_id: f.repository,
                run_id: &f.run,
                locale: "fr",
                source_revision: "revision",
                candidate_commit: "commit",
                policy_fingerprint: &f.policy,
                files: &files,
            },
            authorization_key: &key,
            outbox_id: entry.id,
            owner: "owner",
            payload: &payload,
            state: PublicationState::PushPending,
        })
        .unwrap_err();
    assert!(error.to_string().contains("immutable canonical bytes"));
    assert_eq!(count(&f, "publication_manifests"), 0);
}

#[test]
fn adoption_failure_rolls_back_newly_trusted_translation_memory() {
    let f = fixture();
    let unit = fani::domain::markdown::extract_units("Title").remove(0);
    let units = [AdoptUnitInput {
        unit_key: "title".into(),
        ordinal: 0,
        source_text: unit.source.clone(),
        source_hash: "title-hash".into(),
        context_json: fani::domain::document::unit_metadata(&unit, "guide.md"),
        context_key: unit.memory_context_key("guide.md"),
        target_text: "Titre".into(),
    }];
    trigger(
        &f,
        "CREATE TRIGGER fail_adoption BEFORE UPDATE OF status ON work_items WHEN NEW.status='succeeded' BEGIN SELECT RAISE(ABORT,'injected trust rollback'); END;",
    );
    let mut input = canonical(&f, "Titre");
    input.trust_tier = MemoryTier::Trusted;
    input.review = ReviewState::Approved;
    input.provenance = TranslationProvenance::Human;
    let error =
        f.db.adopt_document(AdoptDocumentInput {
            canonical: input,
            run_id: &f.run,
            identity: &f.identity,
            units: &units,
            metadata: &SourceDocumentMetadata {
                format: DocumentFormat::Markdown,
                contract: f.identity.contract.clone(),
            },
        })
        .unwrap_err();
    assert!(
        error.to_string().contains("injected trust rollback"),
        "{error:#}"
    );
    for table in [
        "units",
        "unit_versions",
        "translation_versions",
        "translation_memory_entries",
        "canonical_files",
        "canonical_file_translations",
        "work_items",
    ] {
        assert_eq!(count(&f, table), 0, "{table}");
    }
}
