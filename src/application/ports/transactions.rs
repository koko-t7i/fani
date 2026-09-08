use super::*;
use crate::application::contracts::{
    DocumentIdentity, MaterializationWorkResult, PipelineRequest, PublicationPayload,
};

pub struct PreparedDocumentInput<'a> {
    pub repository_id: i64,
    pub run_id: &'a str,
    pub locale: &'a str,
    pub path: &'a str,
    pub source_revision: &'a str,
    pub content_hash: &'a str,
    pub metadata_json: &'a str,
    pub units: &'a [PreparedUnitInput],
}
pub struct PreparedUnitInput {
    pub unit_key: String,
    pub ordinal: i64,
    pub source_text: String,
    pub source_hash: String,
    pub context_json: String,
    pub context_key: String,
    pub candidate: Option<TranslationCandidate>,
    pub enqueue: Option<PipelineRequest>,
}
pub struct PreparedDocumentReceipt {
    pub document_id: i64,
    pub units: Vec<PreparedUnitReceipt>,
}
pub struct PreparedUnitReceipt {
    pub unit_id: i64,
    pub work_item_id: Option<i64>,
}
pub struct CanonicalMaterializationInput<'a> {
    pub canonical: CanonicalFileInput<'a>,
    pub translations: &'a [CanonicalTranslationInput<'a>],
    pub identity: &'a DocumentIdentity,
    pub run_id: &'a str,
    pub document_id: i64,
    pub base_dedupe_key: &'a str,
    pub observed_target_matches: bool,
}
pub struct CanonicalMaterializationReceipt {
    pub canonical: CanonicalFile,
    pub dedupe_key: String,
    pub materialization: Option<MaterializationReceipt>,
}
pub struct MaterializationSettlementInput<'a> {
    pub outbox_id: i64,
    pub owner: &'a str,
    pub canonical_file_id: i64,
    pub work_item_id: i64,
    pub transition: Option<CanonicalTransition>,
    pub materialized_hash: Option<&'a str>,
    pub succeeded: bool,
    pub result: &'a MaterializationWorkResult,
}
pub struct PublicationCandidateInput<'a> {
    pub manifest: PublicationManifestInput<'a>,
    pub authorization_key: &'a str,
    pub outbox_id: i64,
    pub owner: &'a str,
    pub payload: &'a PublicationPayload,
    pub state: PublicationState,
}
pub struct PublicationPromotionInput<'a> {
    pub candidate_commit: &'a str,
    pub provenance: &'a str,
    pub verified_zero_unit_contents: &'a [i64],
}
pub struct PublicationSettlementInput<'a> {
    pub repository_id: i64,
    pub locale: &'a str,
    pub candidate_commit: Option<&'a str>,
    pub authorization_key: &'a str,
    pub outbox_id: i64,
    pub owner: &'a str,
    pub payload: Option<&'a PublicationPayload>,
    pub state: Option<PublicationState>,
    pub pull_request: Option<PullRequestStateInput<'a>>,
    pub promotion: Option<PublicationPromotionInput<'a>>,
}
pub struct PublicationObservationInput<'a> {
    pub pull_request: PullRequestStateInput<'a>,
    pub locale: &'a str,
    pub promotion: Option<PublicationPromotionInput<'a>>,
    pub state: Option<PublicationState>,
}
pub struct PublicationIntentInput<'a> {
    pub repository_id: i64,
    pub run_id: &'a str,
    pub locale: &'a str,
    pub base_dedupe_key: &'a str,
    pub payload: &'a PublicationPayload,
}
#[derive(Serialize)]
pub struct SourceDocumentMetadata {
    pub format: crate::domain::document::DocumentFormat,
    pub contract: crate::domain::document::FormatContract,
}
pub struct AdoptUnitInput {
    pub unit_key: String,
    pub ordinal: i64,
    pub source_text: String,
    pub source_hash: String,
    pub context_json: String,
    pub context_key: String,
    pub target_text: String,
}
pub struct AdoptDocumentInput<'a> {
    pub canonical: CanonicalFileInput<'a>,
    pub run_id: &'a str,
    pub identity: &'a DocumentIdentity,
    pub units: &'a [AdoptUnitInput],
    pub metadata: &'a SourceDocumentMetadata,
}
pub struct DiscardIntentInput<'a> {
    pub repository_id: i64,
    pub run_id: &'a str,
    pub document_id: i64,
    pub locale: &'a str,
    pub path: &'a str,
    pub canonical_file_id: i64,
    pub canonical_content_version_id: i64,
    pub content_hash: &'a str,
    pub observed_hash: Option<&'a str>,
}
pub struct DiscardReceipt {
    pub materialization: MaterializationReceipt,
    pub expected_hash: Option<String>,
}

pub trait PreparationStore {
    fn prepare_document(&self, input: PreparedDocumentInput<'_>)
    -> Result<PreparedDocumentReceipt>;
}
pub trait PipelineStore {
    fn attempt_status(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<crate::application::contracts::AttemptStatus>>;
    fn failed_attempt_context(&self, work_item_id: i64) -> Result<Option<FailedAttemptContext>>;
    fn record_attempt(&self, input: AttemptInput<'_>) -> Result<AttemptReceipt>;
    fn record_attempt_candidate(&self, input: AttemptCandidateInput<'_>) -> Result<AttemptReceipt>;
    fn record_finding(&self, input: FindingInput<'_>) -> Result<i64>;
    fn retire_attempt(&self, attempt_id: i64) -> Result<()>;
    fn select_canonical_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate_key: &str,
        target_text: &str,
        source_attempt_id: Option<i64>,
        score: Option<f64>,
    ) -> Result<i64>;
    fn successful_attempt(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<RecoveredAttempt>>;
}
pub trait MaterializationStore {
    fn cancel_materialization(&self, repository_id: i64, id: i64) -> Result<()>;
    fn canonical_compatible(&self, content_version_id: i64) -> Result<bool>;
    fn canonical_content_matches(
        &self,
        repository_id: i64,
        locale: &str,
        source_revision: &str,
        binding: &PublicationManifestFile,
        file: &PublicationFile,
    ) -> Result<bool>;
    fn claim_outbox_key(
        &self,
        kind: OutboxKind,
        dedupe_key: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>>;
    fn pending_materializations(
        &self,
        repository_id: i64,
        locale: &str,
    ) -> Result<Vec<OutboxEntry>>;
    fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool>;
    fn commit_materialization(
        &self,
        input: CanonicalMaterializationInput<'_>,
    ) -> Result<CanonicalMaterializationReceipt>;
    fn settle_materialization(&self, input: MaterializationSettlementInput<'_>) -> Result<()>;
}
pub trait PublicationWorkflowStore {
    fn canonical_compatible(&self, content_version_id: i64) -> Result<bool>;
    fn canonical_content_matches(
        &self,
        repository_id: i64,
        locale: &str,
        source_revision: &str,
        binding: &PublicationManifestFile,
        file: &PublicationFile,
    ) -> Result<bool>;
    fn canonical_document_intent(&self, content_version_id: i64) -> Result<Option<String>>;
    fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>>;
    fn claim_publication_locale(
        &self,
        repository_id: i64,
        locale: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>>;
    fn publication_snapshot(
        &self,
        repository_id: i64,
        locale: &str,
        commit: &str,
    ) -> Result<Vec<CanonicalSnapshot>>;
    fn pull_request_for_branch(
        &self,
        repository_id: i64,
        provider: &str,
        branch: &str,
    ) -> Result<Option<StoredPullRequest>>;
    fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool>;
    fn schedule_publication(&self, input: PublicationIntentInput<'_>) -> Result<String>;
    fn persist_publication_candidate(&self, input: PublicationCandidateInput<'_>) -> Result<()>;
    fn settle_publication(&self, input: PublicationSettlementInput<'_>) -> Result<()>;
    fn observe_publication(&self, input: PublicationObservationInput<'_>) -> Result<()>;
}
pub trait ReconciliationStore: PlanningStore {
    fn begin_run(
        &self,
        repository_id: i64,
        invocation_key: &str,
        config_path: &Path,
        metadata_json: &str,
        policy_fingerprint: &str,
    ) -> Result<String>;
    fn finish_run(&self, run_id: &str, status: &str) -> Result<bool>;
    fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64>;
    fn adopt_document(&self, input: AdoptDocumentInput<'_>) -> Result<CanonicalFile>;
    fn begin_discard(&self, input: DiscardIntentInput<'_>) -> Result<DiscardReceipt>;
    fn complete_discard(
        &self,
        receipt: &MaterializationReceipt,
        canonical_file_id: i64,
        hash: &str,
    ) -> Result<()>;
}
