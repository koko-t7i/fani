//! Business transactions: side effects run outside these SQLite transactions.
use super::*;
use crate::application::contracts::MaterializationWorkResult;
use serde_json::json;

fn require_run(conn: &Connection, repository_id: i64, run_id: &str) -> Result<()> {
    let valid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE id=?1 AND repository_id=?2)",
        params![run_id, repository_id],
        |row| row.get(0),
    )?;
    if !valid {
        bail!("run does not belong to the requested repository");
    }
    Ok(())
}

fn require_publication_owner(
    conn: &Connection,
    id: i64,
    owner: &str,
    repository_id: i64,
    locale: &str,
    key: &str,
) -> Result<()> {
    let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM publication_outbox WHERE id=?1 AND owner=?2 AND state='processing' AND repository_id=?3 AND locale=?4 AND dedupe_key=?5)", params![id,owner,repository_id,locale,key], |row| row.get(0))?;
    if !valid {
        bail!("publication outbox ownership or authorization changed");
    }
    Ok(())
}

impl PreparationStore for Database {
    fn prepare_document(
        &self,
        input: PreparedDocumentInput<'_>,
    ) -> Result<PreparedDocumentReceipt> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_run(&tx, input.repository_id, input.run_id)?;
        let policy: String = tx.query_row(
            "SELECT policy_fingerprint FROM runs WHERE id=?1",
            [input.run_id],
            |row| row.get(0),
        )?;
        let document_id = preparation::upsert_document_in_transaction(
            &tx,
            input.repository_id,
            input.path,
            Some(input.source_revision),
            input.content_hash,
            input.metadata_json,
        )?;
        preparation::quarantine_incompatible_translations_in_transaction(
            &tx,
            document_id,
            input.locale,
        )?;
        let mut units = Vec::with_capacity(input.units.len());
        for unit in input.units {
            let unit_id = preparation::upsert_unit_in_transaction(
                &tx,
                document_id,
                &unit.unit_key,
                unit.ordinal,
                &unit.source_text,
                &unit.source_hash,
                &unit.context_json,
            )?;
            if let Some(candidate) = &unit.candidate {
                if candidate.trusted {
                    canonical::trust_translation_in_transaction(
                        &tx,
                        TrustTranslationInput {
                            repository_id: input.repository_id,
                            unit_id: Some(unit_id),
                            locale: input.locale,
                            source_hash: &unit.source_hash,
                            context_key: &unit.context_key,
                            target_text: &candidate.text,
                            provenance: "compatible_memory",
                            policy_fingerprint: &policy,
                        },
                    )?;
                } else if crate::domain::document::compatible_metadata(&candidate.provenance)
                    .is_some_and(|metadata| metadata.needs_markdown_snapshot_upgrade())
                {
                    preparation::revalidate_candidate_in_transaction(
                        &tx,
                        unit_id,
                        input.locale,
                        candidate,
                    )?;
                }
            }
            let work_item_id = unit
                .enqueue
                .as_ref()
                .map(|request| -> Result<i64> {
                    if request.source_revision != input.source_revision
                        || request.path != input.path
                        || request.unit != unit.unit_key
                    {
                        bail!("pipeline request does not match prepared source unit");
                    }
                    preparation::enqueue_work_item_in_transaction(
                        &tx,
                        input.run_id,
                        unit_id,
                        input.locale,
                        "pipeline",
                        0,
                        &serde_json::to_string(request)?,
                    )
                })
                .transpose()?;
            units.push(PreparedUnitReceipt {
                unit_id,
                work_item_id,
            });
        }
        crate::adapters::failpoint::reach("prepared_document_before_commit");
        tx.commit()?;
        Ok(PreparedDocumentReceipt { document_id, units })
    }
}

impl MaterializationStore for Database {
    fn cancel_materialization(&self, repository_id: i64, id: i64) -> Result<()> {
        Database::cancel_materialization(self, repository_id, id)
    }
    fn canonical_compatible(&self, content_version_id: i64) -> Result<bool> {
        Database::canonical_compatible(self, content_version_id)
    }
    fn canonical_content_matches(
        &self,
        repository_id: i64,
        locale: &str,
        source_revision: &str,
        binding: &PublicationManifestFile,
        file: &PublicationFile,
    ) -> Result<bool> {
        Database::canonical_content_matches(
            self,
            repository_id,
            locale,
            source_revision,
            binding,
            file,
        )
    }
    fn claim_outbox_key(
        &self,
        kind: OutboxKind,
        dedupe_key: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        Database::claim_outbox_key(self, kind, dedupe_key, owner, now, lease_ms)
    }
    fn pending_materializations(
        &self,
        repository_id: i64,
        locale: &str,
    ) -> Result<Vec<OutboxEntry>> {
        Database::pending_materializations(self, repository_id, locale)
    }
    fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool> {
        Database::retry_outbox(self, kind, id, owner, error, available_at)
    }
    fn commit_materialization(
        &self,
        input: CanonicalMaterializationInput<'_>,
    ) -> Result<CanonicalMaterializationReceipt> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository_id = input.canonical.repository_id;
        let locale = input.canonical.locale;
        let path = input.canonical.path;
        let content_hash = input.canonical.content_hash;
        require_run(&tx, repository_id, input.run_id)?;
        if input.identity.source_revision != input.canonical.source_revision
            || input.identity.target_path != path
            || input.identity.locale != locale
            || input.identity.policy_fingerprint != input.canonical.policy_fingerprint
        {
            bail!("canonical content does not match document identity");
        }
        let bound: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM documents WHERE id=?1 AND repository_id=?2 AND path=?3 AND source_revision=?4 AND content_hash=?5)",params![input.document_id,repository_id,input.identity.source_path,input.identity.source_revision,input.identity.source_hash],|row|row.get(0))?;
        if !bound {
            bail!("canonical document identity does not match prepared source document");
        }
        let identity_json = serde_json::to_string(input.identity)?;
        let canonical = canonical::persist_canonical_document_inner_in_transaction(
            &tx,
            input.canonical,
            input.translations,
            Some(&identity_json),
        )?;
        crate::adapters::failpoint::reach("canonical_persisted_before_outbox");
        let dedupe_key = effects::effect_key_in_transaction(
            &tx,
            OutboxKind::Materialization,
            input.base_dedupe_key,
        )?;
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM materialization_outbox WHERE dedupe_key=?1 AND state<>'done')", [&dedupe_key], |row| row.get(0))?;
        let materialization = if !pending
            && dedupe_key != input.base_dedupe_key
            && input.observed_target_matches
        {
            canonical::transition_canonical_file_in_transaction(
                &tx,
                canonical.id,
                CanonicalTransition::Materialized,
                Some(content_hash),
            )?;
            None
        } else {
            Some(effects::schedule_materialization_in_transaction(&tx,MaterializationIntentInput {
                repository_id,run_id:input.run_id,document_id:input.document_id,locale,path,dedupe_key:&dedupe_key,
                work_input_json:&json!({"document_identity":input.identity,"content_hash":content_hash,"effect_key":dedupe_key}).to_string(),
                payload_json:&json!({"repository_id":repository_id,"locale":locale,"path":path,"document_identity":input.identity,"content_hash":content_hash,"canonical_content_version_id":canonical.content_version_id,"canonical_file_id":canonical.id}).to_string(),
            })?)
        };
        tx.commit()?;
        Ok(CanonicalMaterializationReceipt {
            canonical,
            dedupe_key,
            materialization,
        })
    }

    fn settle_materialization(&self, input: MaterializationSettlementInput<'_>) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM materialization_outbox o JOIN work_items w ON w.id=o.work_item_id JOIN runs r ON r.id=w.run_id JOIN canonical_files c ON c.id=?4 AND c.repository_id=r.repository_id AND c.locale=w.locale WHERE o.id=?1 AND o.owner=?2 AND o.state='processing' AND o.work_item_id=?3 AND json_extract(o.payload_json,'$.path')=c.path)", params![input.outbox_id,input.owner,input.work_item_id,input.canonical_file_id], |row| row.get(0))?;
        if !valid {
            bail!("materialization receipt ownership or canonical binding changed");
        }
        if let Some(transition) = input.transition {
            let bound: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM materialization_outbox o JOIN canonical_files c ON c.id=?2 JOIN canonical_content_versions v ON v.id=json_extract(o.payload_json,'$.canonical_content_version_id') AND v.canonical_file_id=c.id WHERE o.id=?1 AND json_extract(o.payload_json,'$.canonical_file_id')=c.id AND json_extract(o.payload_json,'$.content_hash')=v.content_hash AND c.content_hash=v.content_hash AND c.content=v.content)",params![input.outbox_id,input.canonical_file_id],|row|row.get(0))?;
            if !bound {
                bail!("materialization receipt no longer binds current canonical bytes");
            }
            if matches!(transition, CanonicalTransition::Materialized) {
                let matches: bool = tx
                    .query_row(
                        "SELECT content_hash=?2 FROM canonical_files WHERE id=?1",
                        params![input.canonical_file_id, input.materialized_hash],
                        |row| row.get::<_, Option<bool>>(0),
                    )?
                    .unwrap_or(false);
                if !matches {
                    bail!("materialized hash differs from canonical receipt");
                }
            }
            canonical::transition_canonical_file_in_transaction(
                &tx,
                input.canonical_file_id,
                transition,
                input.materialized_hash,
            )?;
        }
        effects::finish_document_work_in_transaction(
            &tx,
            input.work_item_id,
            input.succeeded,
            &serde_json::to_string(input.result)?,
        )?;
        crate::adapters::failpoint::reach("materialization_state_transitioned");
        if !effects::complete_outbox_in_transaction(
            &tx,
            OutboxKind::Materialization,
            input.outbox_id,
            input.owner,
        )? {
            bail!("materialization lease lost before completion");
        }
        tx.commit()?;
        Ok(())
    }
}

impl PublicationWorkflowStore for Database {
    fn canonical_compatible(&self, content_version_id: i64) -> Result<bool> {
        Database::canonical_compatible(self, content_version_id)
    }
    fn canonical_content_matches(
        &self,
        repository_id: i64,
        locale: &str,
        source_revision: &str,
        binding: &PublicationManifestFile,
        file: &PublicationFile,
    ) -> Result<bool> {
        Database::canonical_content_matches(
            self,
            repository_id,
            locale,
            source_revision,
            binding,
            file,
        )
    }
    fn canonical_document_intent(&self, content_version_id: i64) -> Result<Option<String>> {
        Database::canonical_document_intent(self, content_version_id)
    }
    fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>> {
        PlanningStore::canonical_file(self, repository_id, locale, path)
    }
    fn claim_publication_locale(
        &self,
        repository_id: i64,
        locale: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>> {
        Database::claim_publication_locale(self, repository_id, locale, owner, now, lease_ms)
    }
    fn publication_snapshot(
        &self,
        repository_id: i64,
        locale: &str,
        commit: &str,
    ) -> Result<Vec<CanonicalSnapshot>> {
        Database::publication_snapshot(self, repository_id, locale, commit)
    }
    fn pull_request_for_branch(
        &self,
        repository_id: i64,
        provider: &str,
        branch: &str,
    ) -> Result<Option<StoredPullRequest>> {
        Database::pull_request_for_branch(self, repository_id, provider, branch)
    }
    fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool> {
        Database::retry_outbox(self, kind, id, owner, error, available_at)
    }
    fn schedule_publication(&self, input: PublicationIntentInput<'_>) -> Result<String> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_run(&tx, input.repository_id, input.run_id)?;
        if input.payload.run_id != input.run_id || input.payload.language != input.locale {
            bail!("publication payload does not match run and locale");
        }
        let key = effects::effect_key_in_transaction(
            &tx,
            OutboxKind::Publication,
            input.base_dedupe_key,
        )?;
        publication::enqueue_publication_in_transaction(
            &tx,
            input.repository_id,
            Some(input.run_id),
            input.locale,
            &key,
            &serde_json::to_string(input.payload)?,
        )?;
        tx.commit()?;
        Ok(key)
    }

    fn persist_publication_candidate(&self, input: PublicationCandidateInput<'_>) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let manifest = &input.manifest;
        require_publication_owner(
            &tx,
            input.outbox_id,
            input.owner,
            manifest.repository_id,
            manifest.locale,
            input.authorization_key,
        )?;
        if input.payload.commit.as_deref() != Some(manifest.candidate_commit)
            || input.payload.run_id != manifest.run_id
            || input.payload.language != manifest.locale
            || input.payload.source_revision != manifest.source_revision
            || input.payload.policy_fingerprint != manifest.policy_fingerprint
        {
            bail!("publication candidate payload conflicts with authorization");
        }
        if input.payload.files.len() != manifest.files.len() {
            bail!("publication payload file set differs from authorization");
        }
        let mut seen = std::collections::HashSet::new();
        for file in &input.payload.files {
            if !seen.insert(file.canonical_content_version_id)
                || !manifest.files.iter().any(|binding| {
                    binding.canonical_content_version_id == file.canonical_content_version_id
                        && binding.canonical_file_id == file.canonical_file_id
                        && binding.content_hash == file.content_hash
                })
            {
                bail!("publication payload file set differs from authorization");
            }
            let bound: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM canonical_content_versions v JOIN canonical_files c ON c.id=v.canonical_file_id WHERE v.id=?1 AND c.id=?2 AND v.content_hash=?3 AND v.content=?4 AND c.path=?5)",params![file.canonical_content_version_id,file.canonical_file_id,file.content_hash,file.content.as_bytes(),file.path],|row|row.get(0))?;
            if !bound {
                bail!("publication payload differs from immutable canonical bytes");
            }
        }
        let repository_id = manifest.repository_id;
        let locale = manifest.locale;
        let commit = manifest.candidate_commit;
        publication::record_publication_authorization_in_transaction(
            &tx,
            input.manifest,
            input.authorization_key,
        )?;
        publication::transition_publication_authorization_in_transaction(
            &tx,
            repository_id,
            locale,
            commit,
            Some(input.authorization_key),
            input.state,
        )?;
        if !effects::update_outbox_payload_in_transaction(
            &tx,
            OutboxKind::Publication,
            input.outbox_id,
            input.owner,
            &serde_json::to_string(input.payload)?,
        )? {
            bail!("publication ownership changed before candidate persistence");
        }
        crate::adapters::failpoint::reach("publication_candidate_before_commit");
        tx.commit()?;
        Ok(())
    }

    fn settle_publication(&self, input: PublicationSettlementInput<'_>) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_publication_owner(
            &tx,
            input.outbox_id,
            input.owner,
            input.repository_id,
            input.locale,
            input.authorization_key,
        )?;
        let durable_commit: Option<String> = tx.query_row(
            "SELECT json_extract(payload_json,'$.commit') FROM publication_outbox WHERE id=?1",
            [input.outbox_id],
            |row| row.get(0),
        )?;
        if durable_commit.as_deref() != input.candidate_commit {
            bail!("publication settlement commit differs from durable candidate");
        }
        if let Some(payload) = input.payload {
            if payload.commit.as_deref() != input.candidate_commit
                || payload.language != input.locale
            {
                bail!("publication settlement payload differs from durable candidate");
            }
        }
        if let Some(pull) = input.pull_request {
            if pull.repository_id != input.repository_id
                || pull.head_revision != input.candidate_commit
            {
                bail!("pull request does not match publication candidate");
            }
            publication::record_pr_state_in_transaction(&tx, pull)?;
        }
        if let Some(promotion) = input.promotion {
            if Some(promotion.candidate_commit) != input.candidate_commit {
                bail!("promotion does not match publication candidate");
            }
            publication::promote_publication_in_transaction(
                &tx,
                input.repository_id,
                input.locale,
                promotion.candidate_commit,
                promotion.provenance,
                promotion.verified_zero_unit_contents,
            )?;
        } else if let (Some(commit), Some(state)) = (input.candidate_commit, input.state) {
            publication::transition_publication_authorization_in_transaction(
                &tx,
                input.repository_id,
                input.locale,
                commit,
                Some(input.authorization_key),
                state,
            )?;
        }
        if let Some(payload) = input.payload {
            if !effects::update_outbox_payload_in_transaction(
                &tx,
                OutboxKind::Publication,
                input.outbox_id,
                input.owner,
                &serde_json::to_string(payload)?,
            )? {
                bail!("publication payload ownership changed");
            }
        }
        crate::adapters::failpoint::reach("publication_settlement_before_receipt");
        if !effects::complete_outbox_in_transaction(
            &tx,
            OutboxKind::Publication,
            input.outbox_id,
            input.owner,
        )? {
            bail!("publication lease lost before completion");
        }
        tx.commit()?;
        Ok(())
    }

    fn observe_publication(&self, input: PublicationObservationInput<'_>) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository_id = input.pull_request.repository_id;
        let commit = input.pull_request.head_revision;
        if let Some(promotion) = input.promotion {
            if commit != Some(promotion.candidate_commit) {
                bail!("observed pull request does not match verified merged candidate");
            }
            publication::promote_publication_in_transaction(
                &tx,
                repository_id,
                input.locale,
                promotion.candidate_commit,
                promotion.provenance,
                promotion.verified_zero_unit_contents,
            )?;
        } else if let (Some(commit), Some(state)) = (commit, input.state) {
            publication::transition_publication_authorization_in_transaction(
                &tx,
                repository_id,
                input.locale,
                commit,
                None,
                state,
            )?;
        }
        publication::record_pr_state_in_transaction(&tx, input.pull_request)?;
        tx.commit()?;
        Ok(())
    }
}

impl ReconciliationStore for Database {
    fn begin_run(
        &self,
        repository_id: i64,
        invocation_key: &str,
        config_path: &Path,
        metadata_json: &str,
        policy_fingerprint: &str,
    ) -> Result<String> {
        Database::begin_run(
            self,
            repository_id,
            invocation_key,
            config_path,
            metadata_json,
            policy_fingerprint,
        )
    }
    fn finish_run(&self, run_id: &str, status: &str) -> Result<bool> {
        Database::finish_run(self, run_id, status)
    }
    fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64> {
        Database::upsert_repository(self, repository_key, root_path, default_branch, remote_url)
    }
    fn adopt_document(&self, input: AdoptDocumentInput<'_>) -> Result<CanonicalFile> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let repository_id = input.canonical.repository_id;
        let locale = input.canonical.locale;
        let hash = input.canonical.content_hash;
        require_run(&tx, repository_id, input.run_id)?;
        if input.identity.source_revision != input.canonical.source_revision
            || input.identity.target_path != input.canonical.path
            || input.identity.locale != locale
            || input.identity.policy_fingerprint != input.canonical.policy_fingerprint
            || input.metadata.contract != input.identity.contract
            || input.metadata.format != input.identity.contract.format
        {
            bail!("adoption source identity conflicts with canonical document contract");
        }
        let document_id = preparation::upsert_document_in_transaction(
            &tx,
            repository_id,
            &input.identity.source_path,
            Some(&input.identity.source_revision),
            &input.identity.source_hash,
            &serde_json::to_string(input.metadata)?,
        )?;
        let identity = serde_json::to_string(input.identity)?;
        let assembly_work_id = enqueue_document_work_item_in_transaction(
            &tx,
            input.run_id,
            document_id,
            locale,
            "assembly",
            0,
            &identity,
        )?;
        let mut links = Vec::with_capacity(input.units.len());
        for unit in input.units {
            let unit_id = preparation::upsert_unit_in_transaction(
                &tx,
                document_id,
                &unit.unit_key,
                unit.ordinal,
                &unit.source_text,
                &unit.source_hash,
                &unit.context_json,
            )?;
            canonical::trust_translation_in_transaction(
                &tx,
                TrustTranslationInput {
                    repository_id,
                    unit_id: Some(unit_id),
                    locale,
                    source_hash: &unit.source_hash,
                    context_key: &unit.context_key,
                    target_text: &unit.target_text,
                    provenance: "human_adopted",
                    policy_fingerprint: input.canonical.policy_fingerprint,
                },
            )?;
            links.push(CanonicalTranslationInput {
                unit_id,
                target_text: &unit.target_text,
            });
        }
        let identity = serde_json::to_string(input.identity)?;
        let canonical = canonical::persist_canonical_document_inner_in_transaction(
            &tx,
            input.canonical,
            &links,
            Some(&identity),
        )?;
        canonical::transition_canonical_file_in_transaction(
            &tx,
            canonical.id,
            CanonicalTransition::Adopted,
            Some(hash),
        )?;
        let result = serde_json::to_string(&MaterializationWorkResult {
            status: crate::application::contracts::MaterializationStatus::Adopted,
            content_hash: hash.into(),
            code: None,
        })?;
        effects::finish_document_work_in_transaction(&tx, assembly_work_id, true, &result)?;
        let work = enqueue_document_work_item_in_transaction(
            &tx,
            input.run_id,
            document_id,
            locale,
            "materialization",
            0,
            &identity,
        )?;
        effects::finish_document_work_in_transaction(&tx, work, true, &result)?;
        crate::adapters::failpoint::reach("adopt_document_before_commit");
        tx.commit()?;
        Ok(canonical)
    }

    fn begin_discard(&self, input: DiscardIntentInput<'_>) -> Result<DiscardReceipt> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_run(&tx, input.repository_id, input.run_id)?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM canonical_files c JOIN canonical_content_versions v ON v.id=c.current_content_version_id WHERE c.id=?1 AND v.id=?2 AND c.repository_id=?3 AND c.locale=?4 AND c.path=?5 AND v.content_hash=?6)", params![input.canonical_file_id,input.canonical_content_version_id,input.repository_id,input.locale,input.path,input.content_hash], |row| row.get(0))?;
        if !valid {
            bail!("discard intent does not match current canonical content");
        }
        let base = format!(
            "discard:{}:{}:{}:{}",
            input.repository_id, input.locale, input.path, input.canonical_content_version_id
        );
        let key = effects::effect_key_in_transaction(&tx, OutboxKind::Materialization, &base)?;
        let receipt = effects::schedule_materialization_in_transaction(&tx,MaterializationIntentInput {
            repository_id:input.repository_id,run_id:input.run_id,document_id:input.document_id,locale:input.locale,path:input.path,dedupe_key:&key,
            work_input_json:&json!({"effect_key":key,"operation":"discard"}).to_string(),
            payload_json:&json!({"repository_id":input.repository_id,"locale":input.locale,"path":input.path,"canonical_file_id":input.canonical_file_id,"canonical_content_version_id":input.canonical_content_version_id,"content_hash":input.content_hash,"operation":"discard","expected_hash":input.observed_hash}).to_string(),
        })?;
        let expected_hash: Option<String> = tx.query_row("SELECT json_extract(payload_json,'$.expected_hash') FROM materialization_outbox WHERE id=?1", [receipt.outbox_id], |row| row.get(0))?;
        tx.commit()?;
        Ok(DiscardReceipt {
            materialization: receipt,
            expected_hash,
        })
    }

    fn complete_discard(
        &self,
        receipt: &MaterializationReceipt,
        canonical_file_id: i64,
        hash: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM materialization_outbox o JOIN canonical_files c ON c.id=?3 WHERE o.id=?1 AND o.work_item_id=?2 AND o.state='pending' AND json_extract(o.payload_json,'$.operation')='discard' AND json_extract(o.payload_json,'$.canonical_file_id')=c.id AND json_extract(o.payload_json,'$.canonical_content_version_id')=c.current_content_version_id AND json_extract(o.payload_json,'$.content_hash')=?4)", params![receipt.outbox_id,receipt.work_item_id,canonical_file_id,hash], |row| row.get(0))?;
        if !valid {
            bail!("discard receipt does not match current canonical content");
        }
        canonical::transition_canonical_file_in_transaction(
            &tx,
            canonical_file_id,
            CanonicalTransition::Materialized,
            Some(hash),
        )?;
        effects::finish_document_work_in_transaction(
            &tx,
            receipt.work_item_id,
            true,
            &json!({"status":"discarded","content_hash":hash}).to_string(),
        )?;
        crate::adapters::failpoint::reach("discard_state_before_receipt");
        tx.execute(
            "UPDATE materialization_outbox SET state='done',completed_at=?2 WHERE id=?1",
            params![receipt.outbox_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

impl PipelineStore for Database {
    fn attempt_status(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<crate::application::contracts::AttemptStatus>> {
        Database::attempt_status(self, work_item_id, dedupe_key)
    }
    fn failed_attempt_context(&self, work_item_id: i64) -> Result<Option<FailedAttemptContext>> {
        Database::failed_attempt_context(self, work_item_id)
    }
    fn record_attempt(&self, input: AttemptInput<'_>) -> Result<AttemptReceipt> {
        Database::record_attempt(self, input)
    }
    fn record_attempt_candidate(&self, input: AttemptCandidateInput<'_>) -> Result<AttemptReceipt> {
        Database::record_attempt_candidate(self, input)
    }
    fn record_finding(&self, input: FindingInput<'_>) -> Result<i64> {
        Database::record_finding(self, input)
    }
    fn retire_attempt(&self, attempt_id: i64) -> Result<()> {
        Database::retire_attempt(self, attempt_id)
    }
    fn select_canonical_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate_key: &str,
        target_text: &str,
        source_attempt_id: Option<i64>,
        score: Option<f64>,
    ) -> Result<i64> {
        Database::select_canonical_candidate(
            self,
            unit_id,
            locale,
            candidate_key,
            target_text,
            source_attempt_id,
            score,
        )
    }
    fn successful_attempt(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<RecoveredAttempt>> {
        Database::successful_attempt(self, work_item_id, dedupe_key)
    }
}
