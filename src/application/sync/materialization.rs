use crate::application::contracts::{MaterializationStatus, decode};
use crate::application::ports::{
    CanonicalFileInput, CanonicalTranslationInput, Materialization, MaterializationResult,
    Materializer, OutboxKind, PublicationFile, PublicationManifestFile, SourceReader,
};
use crate::application::settings::RepoConfig;
use crate::domain::model::{
    CanonicalTransition, DecisionCode, Finding, Freshness, MemoryTier, PublicationState,
    ReviewState, TranslationProvenance, ValidationState,
};
use crate::domain::prompts;
use anyhow::Result;
use chrono::Utc;
use std::path::Path;
use std::time::Instant;

use crate::application::contracts::MaterializationPayload;
use crate::application::contracts::MaterializationWorkResult;
use crate::application::ports::{
    CanonicalMaterializationInput, MaterializationSettlementInput, MaterializationStore,
};
use crate::application::sync::context::{
    compatible_document_identity, content_hash, document_identity, finding,
};
use crate::application::sync::verification::VerifiedBatch;
pub(crate) struct MaterializationService<'a> {
    pub repo: &'a RepoConfig,
    pub database: &'a dyn MaterializationStore,
    pub materializer: &'a dyn Materializer,
    pub git: &'a dyn SourceReader,
    pub owner_identity: &'a str,
    pub failpoint: fn(&str),
}
impl MaterializationService<'_> {
    pub(crate) fn materialize(
        &self,
        repository_id: i64,
        run_id: &str,
        language: &str,
        source_revision: &str,
        verified: &VerifiedBatch,
    ) -> Result<(Vec<String>, Vec<PublicationFile>, Vec<Finding>)> {
        let mut written = Vec::new();
        let candidates = verified
            .documents()
            .iter()
            .map(|document| document.file().clone())
            .collect();
        let mut findings = Vec::new();
        if !verified.documents().is_empty() {
            (self.failpoint)("project_checks_completed");
        }
        for verified_document in verified.documents() {
            let document = verified_document.document();
            let candidate = verified_document.file();
            let desired = candidate.content.clone();
            let desired_hash = content_hash(&desired);
            let exact_translations = document
                .units
                .iter()
                .map(|unit| CanonicalTranslationInput {
                    unit_id: unit.database_id,
                    target_text: unit.translation.as_deref().expect("checked translation"),
                })
                .collect::<Vec<_>>();
            let base_dedupe = format!(
                "materialize:{repository_id}:{language}:{}:{desired_hash}:{}",
                document.target_path,
                document.identity.mapping_hash(),
            );
            let receipt = self
                .database
                .commit_materialization(CanonicalMaterializationInput {
                    canonical: CanonicalFileInput {
                        repository_id,
                        locale: language,
                        path: &document.target_path,
                        source_revision,
                        content: &desired,
                        content_hash: &desired_hash,
                        materialized_hash: document.expected_materialized_hash.as_deref(),
                        freshness: Freshness::Exact,
                        provenance: if document.units.is_empty() {
                            TranslationProvenance::Imported
                        } else {
                            TranslationProvenance::Ai
                        },
                        validation: ValidationState::Passed,
                        review: ReviewState::Unreviewed,
                        publication: PublicationState::Candidate,
                        trust_tier: MemoryTier::Candidate,
                        policy_fingerprint: &prompts::policy_fingerprint(),
                    },
                    translations: &exact_translations,
                    identity: &document.identity,
                    run_id,
                    document_id: document.document_id,
                    base_dedupe_key: &base_dedupe,
                    observed_target_matches: self
                        .materializer
                        .read(&self.repo.path, Path::new(&document.target_path))?
                        .is_some_and(|bytes| bytes == desired),
                })?;
            let canonical_id = receipt.canonical.id;
            let dedupe = receipt.dedupe_key;
            let Some(receipt) = receipt.materialization else {
                continue;
            };
            let work_item_id = receipt.work_item_id;
            let owner = format!("materialize:{}:{run_id}", self.owner_identity);
            let claimed = self.database.claim_outbox_key(
                OutboxKind::Materialization,
                &dedupe,
                &owner,
                Utc::now().timestamp_millis(),
                60_000,
            )?;
            if let Some(entry) = claimed {
                let payload = decode::<MaterializationPayload>(&entry.payload_json).ok();
                let compatible = if let Some(payload) = payload {
                    payload.repository_id == repository_id
                        && payload.locale == language
                        && payload.path == document.target_path
                        && payload.content_hash == desired_hash
                        && compatible_document_identity(
                            &payload.document_identity,
                            &document.identity,
                        )
                        && self.database.canonical_content_matches(
                            repository_id,
                            language,
                            &payload.document_identity.source_revision,
                            &PublicationManifestFile {
                                canonical_content_version_id: payload.canonical_content_version_id,
                                canonical_file_id: payload.canonical_file_id,
                                content_hash: payload.content_hash.clone(),
                            },
                            candidate,
                        )?
                        && self
                            .database
                            .canonical_compatible(payload.canonical_content_version_id)?
                } else {
                    false
                };
                if !compatible {
                    self.database
                        .settle_materialization(MaterializationSettlementInput {
                            outbox_id: entry.id,
                            owner: &owner,
                            canonical_file_id: canonical_id,
                            work_item_id,
                            transition: None,
                            materialized_hash: None,
                            succeeded: false,
                            result: &MaterializationWorkResult {
                                status: MaterializationStatus::Rejected,
                                content_hash: desired_hash.clone(),
                                code: Some("DOCUMENT-INTENT".into()),
                            },
                        })?;
                    findings.push(finding(
                        &document.target_path,
                        None,
                        "DOCUMENT-INTENT",
                        "stored materialization identity failed current validation",
                    ));
                    continue;
                }
                let outbox_started = Instant::now();
                tracing::info!(
                    event = "outbox.claimed",
                    kind = "materialization",
                    outbox_id = entry.id,
                    run_id = %crate::diagnostics::safe_id(run_id),
                    locale = language,
                    item_id = %crate::diagnostics::safe_id(&dedupe),
                );
                let operation = Materialization {
                    path: document.target_path.clone().into(),
                    expected_hash: document.expected_materialized_hash.clone(),
                    desired,
                };
                (self.failpoint)("materialization_before_file_write");
                let result = self.materializer.apply(&self.repo.path, &operation);
                let (operation_status, transition, materialized_hash) = match result {
                    Ok(MaterializationResult::Written { hash }) => {
                        (self.failpoint)("materialized_file_written");
                        written.push(document.target_path.clone());
                        (
                            MaterializationStatus::Written,
                            CanonicalTransition::Materialized,
                            hash,
                        )
                    }
                    Ok(MaterializationResult::AlreadyCurrent { hash }) => (
                        MaterializationStatus::AlreadyCurrent,
                        CanonicalTransition::Materialized,
                        hash,
                    ),
                    Ok(MaterializationResult::HumanEdit { actual_hash }) => {
                        findings.push(finding(
                            &document.target_path,
                            None,
                            DecisionCode::HumanEdit.as_str(),
                            "target changed outside fani and was not overwritten",
                        ));
                        (
                            MaterializationStatus::HumanEdit,
                            CanonicalTransition::HumanEdit,
                            actual_hash,
                        )
                    }
                    Err(error) => {
                        tracing::warn!(
                            event = "outbox.completed",
                            kind = "materialization",
                            outbox_id = entry.id,
                            run_id = %crate::diagnostics::safe_id(run_id),
                            locale = language,
                            status = "retry",
                            duration_ms = outbox_started.elapsed().as_millis() as u64,
                        );
                        self.database.retry_outbox(
                            OutboxKind::Materialization,
                            entry.id,
                            &owner,
                            &error.to_string(),
                            Utc::now().timestamp_millis() + 1_000,
                        )?;
                        return Err(error);
                    }
                };
                self.database
                    .settle_materialization(MaterializationSettlementInput {
                        outbox_id: entry.id,
                        owner: &owner,
                        canonical_file_id: canonical_id,
                        work_item_id,
                        transition: Some(transition),
                        materialized_hash: Some(&materialized_hash),
                        succeeded: !matches!(operation_status, MaterializationStatus::HumanEdit),
                        result: &MaterializationWorkResult {
                            status: operation_status,
                            content_hash: desired_hash.clone(),
                            code: None,
                        },
                    })?;
                tracing::info!(
                    event = "outbox.completed",
                    kind = "materialization",
                    outbox_id = entry.id,
                    run_id = %crate::diagnostics::safe_id(run_id),
                    locale = language,
                    status = operation_status.as_str(),
                    duration_ms = outbox_started.elapsed().as_millis() as u64,
                );
            }
        }
        Ok((written, candidates, findings))
    }
    pub(crate) fn retire_incompatible_materializations(
        &self,
        repository_id: i64,
        language: &str,
        revision: &str,
    ) -> Result<()> {
        let sources = self.git.discover(self.repo, revision)?;
        for entry in self
            .database
            .pending_materializations(repository_id, language)?
        {
            let payload = decode::<MaterializationPayload>(&entry.payload_json).ok();
            let compatible = payload.as_ref().is_some_and(|payload| {
                payload.repository_id == repository_id
                    && payload.locale == language
                    && sources.iter().any(|source| {
                        source.target_path(language).to_string_lossy() == payload.path
                            && source.parse().is_ok_and(|parsed| {
                                compatible_document_identity(
                                    &payload.document_identity,
                                    &document_identity(source, language, &parsed),
                                )
                            })
                    })
            });
            if !compatible {
                self.database
                    .cancel_materialization(repository_id, entry.id)?;
            }
        }
        Ok(())
    }
}
