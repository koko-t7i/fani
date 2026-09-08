use crate::application::settings::RepoConfig;
use crate::domain::model::{
    AgentResult, AgentTask, CanonicalTransition, Freshness, MemoryTier, PublicationState,
    Published, ReviewState, SourceDocument, TranslationProvenance, ValidationState,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const AGENT_REQUEST_SCHEMA: &str = "fani.agent.request.v2";
pub const AGENT_RESPONSE_SCHEMA: &str = "fani.agent.response.v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPrompt {
    pub version: String,
    pub resource: String,
    pub hash: String,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    pub fingerprint: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRequestEnvelope {
    pub schema: String,
    pub task: AgentTask,
    pub prompt: AgentPrompt,
    pub policy: AgentPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentResponseEnvelope {
    pub schema: String,
    pub task_id: String,
    pub output: String,
}

#[derive(Clone, Debug)]
pub struct AgentExecution {
    pub agent: String,
    pub provider: String,
    pub model: String,
    pub adapter: String,
    pub provider_fingerprint: String,
    pub results: Vec<AgentResult>,
}

pub trait AgentExecutor: Send + Sync {
    fn configuration_fingerprint(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn execute(&self, tasks: &[AgentTask]) -> Result<AgentExecution>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentationCheckFailure {
    pub timed_out: bool,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentationCheck {
    pub failures: Vec<(usize, DocumentationCheckFailure)>,
}

pub trait DocumentationChecker {
    fn check(
        &self,
        repo: &RepoConfig,
        source_revision: &str,
        files: &[PublicationFile],
    ) -> Result<DocumentationCheck>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationFile {
    pub path: String,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationManifestFile {
    pub canonical_content_version_id: i64,
    pub canonical_file_id: i64,
    pub content_hash: String,
}

#[derive(Clone, Debug)]
pub struct PublicationManifestInput<'a> {
    pub repository_id: i64,
    pub run_id: &'a str,
    pub locale: &'a str,
    pub source_revision: &'a str,
    pub candidate_commit: &'a str,
    pub policy_fingerprint: &'a str,
    pub files: &'a [PublicationManifestFile],
}

#[derive(Clone, Debug)]
pub struct PreparedPublication {
    pub published: Published,
    pub expected_remote_tip: Option<String>,
}

pub trait GitPublisher {
    fn resolve_source_revision(&self, repo: &RepoConfig) -> Result<String>;
    fn discover(&self, repo: &RepoConfig, source_revision: &str) -> Result<Vec<SourceDocument>>;
    fn branch(&self, repo: &RepoConfig, language: &str) -> Result<String>;
    fn prepare(
        &self,
        repo: &RepoConfig,
        language: &str,
        source_revision: &str,
        files: &[PublicationFile],
    ) -> Result<PreparedPublication>;
    fn publish_pending(
        &self,
        repo: &RepoConfig,
        language: &str,
        commit: &str,
        expected_remote_tip: Option<&str>,
    ) -> Result<Published>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub state: String,
    pub draft: bool,
    pub head_revision: Option<String>,
    pub payload_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnsurePullRequest<'a> {
    pub repository: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciledPullRequest {
    pub pull_request: PullRequest,
    pub payload_json: String,
}

pub trait CodeHost {
    fn pull_request(
        &self,
        repo: &RepoConfig,
        repository: &str,
        selector: &str,
    ) -> Result<PullRequest>;
    fn ensure_pull_request(
        &self,
        repo: &RepoConfig,
        request: EnsurePullRequest<'_>,
    ) -> Result<ReconciledPullRequest>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Materialization {
    pub path: PathBuf,
    pub expected_hash: Option<String>,
    pub desired: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializationResult {
    Written { hash: String },
    AlreadyCurrent { hash: String },
    HumanEdit { actual_hash: String },
}

pub trait Materializer {
    fn read(&self, root: &Path, relative: &Path) -> Result<Option<Vec<u8>>>;
    fn apply(&self, root: &Path, operation: &Materialization) -> Result<MaterializationResult>;
    fn restore(&self, root: &Path, operation: &Materialization) -> Result<MaterializationResult>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptReceipt {
    pub id: i64,
    pub inserted: bool,
}

#[derive(Clone, Debug)]
pub struct AttemptInput<'a> {
    pub work_item_id: i64,
    pub dedupe_key: &'a str,
    pub agent: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub adapter: &'a str,
    pub provider_fingerprint: &'a str,
    pub prompt_version: &'a str,
    pub prompt_hash: &'a str,
    pub policy_fingerprint: &'a str,
    pub status: &'a str,
    pub request_json: &'a str,
    pub response_json: Option<&'a str>,
    pub error: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct AttemptCandidateInput<'a> {
    pub attempt: AttemptInput<'a>,
    pub unit_id: i64,
    pub locale: &'a str,
    pub candidate_key: &'a str,
    pub target_text: &'a str,
    pub score: Option<f64>,
    pub policy_fingerprint: &'a str,
    pub provenance: TranslationProvenance,
}

#[derive(Clone, Debug)]
pub struct TranslationCandidate {
    pub unit_id: i64,
    pub text: String,
    pub provenance: crate::domain::document::UnitProvenance,
    pub trusted: bool,
    pub run_id: Option<String>,
    pub invocation_key: Option<String>,
    pub deterministic_model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredAttempt {
    pub id: i64,
    pub dedupe_key: String,
    pub request_json: String,
    pub output: String,
    pub provenance: Option<crate::domain::document::UnitProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedAttemptContext {
    pub attempt_id: i64,
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TrustTranslationInput<'a> {
    pub repository_id: i64,
    pub unit_id: Option<i64>,
    pub locale: &'a str,
    pub source_hash: &'a str,
    pub context_key: &'a str,
    pub target_text: &'a str,
    pub provenance: &'a str,
    pub policy_fingerprint: &'a str,
}

#[derive(Clone, Debug)]
pub struct FindingInput<'a> {
    pub work_item_id: i64,
    pub attempt_id: Option<i64>,
    pub finding_key: &'a str,
    pub severity: &'a str,
    pub code: &'a str,
    pub message: &'a str,
    pub details_json: &'a str,
}

#[derive(Clone, Debug)]
pub struct PullRequestStateInput<'a> {
    pub repository_id: i64,
    pub provider: &'a str,
    pub external_id: &'a str,
    pub number: Option<i64>,
    pub branch: &'a str,
    pub url: Option<&'a str>,
    pub state: &'a str,
    pub head_revision: Option<&'a str>,
    pub event_key: &'a str,
    pub payload_json: &'a str,
}

#[derive(Clone, Debug)]
pub struct CanonicalTranslationInput<'a> {
    pub unit_id: i64,
    pub target_text: &'a str,
}

#[derive(Clone, Debug)]
pub struct CanonicalFileInput<'a> {
    pub repository_id: i64,
    pub locale: &'a str,
    pub path: &'a str,
    pub source_revision: &'a str,
    pub content: &'a [u8],
    pub content_hash: &'a str,
    pub materialized_hash: Option<&'a str>,
    pub freshness: Freshness,
    pub provenance: TranslationProvenance,
    pub validation: ValidationState,
    pub review: ReviewState,
    pub publication: PublicationState,
    pub trust_tier: MemoryTier,
    pub policy_fingerprint: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxKind {
    Materialization,
    Publication,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxEntry {
    pub id: i64,
    pub dedupe_key: String,
    pub payload_json: String,
    pub attempt_count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalFile {
    pub id: i64,
    pub content_version_id: i64,
    pub source_revision: String,
    pub content: Vec<u8>,
    pub content_hash: String,
    pub materialized_hash: Option<String>,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct CanonicalSnapshot {
    pub content_version_id: i64,
    pub source_revision: String,
    pub path: String,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredPullRequest {
    pub external_id: String,
    pub number: Option<i64>,
    pub branch: String,
    pub url: Option<String>,
    pub state: String,
    pub head_revision: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnitHistory {
    pub id: i64,
    pub unit_key: String,
    pub ordinal: usize,
    pub source_text: String,
    pub source_hash: String,
    pub context_json: String,
    pub translation: Option<String>,
    pub trusted: bool,
}

/// Durable work and outbox intent committed together, preserving an existing pending receipt.
pub struct MaterializationIntentInput<'a> {
    pub repository_id: i64,
    pub run_id: &'a str,
    pub document_id: i64,
    pub locale: &'a str,
    pub path: &'a str,
    pub dedupe_key: &'a str,
    pub work_input_json: &'a str,
    pub payload_json: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationReceipt {
    pub work_item_id: i64,
    pub outbox_id: i64,
}

pub trait DocumentStore {
    fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64>;
    fn upsert_document(
        &self,
        repository_id: i64,
        path: &str,
        source_revision: Option<&str>,
        content_hash: &str,
        metadata_json: &str,
    ) -> Result<i64>;
    fn document_id(&self, repository_id: i64, path: &str) -> Result<Option<i64>>;
    fn upsert_unit(
        &self,
        document_id: i64,
        unit_key: &str,
        ordinal: i64,
        source_text: &str,
        source_hash: &str,
        context_json: &str,
    ) -> Result<i64>;
    fn unit_history(&self, document_id: i64, locale: &str) -> Result<Vec<UnitHistory>>;
    fn unchanged_document_unit_keys(
        &self,
        repository_id: i64,
        path: &str,
        content_hash: &str,
    ) -> Result<Vec<String>>;
    fn repository_id(&self, repository_key: &str) -> Result<Option<i64>>;
}

pub trait TranslationStore {
    fn translation_candidates(
        &self,
        document_id: i64,
        locale: &str,
    ) -> Result<Vec<TranslationCandidate>>;
    fn review_attempts(
        &self,
        unit_id: i64,
        locale: &str,
        run_or_invocation: &str,
    ) -> Result<Vec<RecoveredAttempt>>;
    fn revalidate_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate: &TranslationCandidate,
    ) -> Result<()>;
    fn retire_attempt(&self, attempt_id: i64) -> Result<()>;
    fn quarantine_incompatible_translations(&self, document_id: i64, locale: &str) -> Result<()>;
    fn trusted_translation(
        &self,
        repository_id: i64,
        locale: &str,
        source_hash: &str,
        context_key: &str,
    ) -> Result<Option<String>>;
    fn trust_translation(&self, input: TrustTranslationInput<'_>) -> Result<i64>;
    fn successful_attempt(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
    ) -> Result<Option<RecoveredAttempt>>;
    fn attempt_status(&self, work_item_id: i64, dedupe_key: &str) -> Result<Option<String>>;
    fn failed_attempt_context(&self, work_item_id: i64) -> Result<Option<FailedAttemptContext>>;
    fn recoverable_candidate(
        &self,
        run_id: &str,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>>;
    fn recoverable_invocation_candidate(
        &self,
        invocation_key: &str,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>>;
    fn recoverable_unit_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        policy_fingerprint: &str,
        deterministic_repair_version: &str,
    ) -> Result<Option<String>>;
    fn record_attempt(&self, input: AttemptInput<'_>) -> Result<AttemptReceipt>;
    fn record_attempt_candidate(&self, input: AttemptCandidateInput<'_>) -> Result<AttemptReceipt>;
    fn select_canonical_candidate(
        &self,
        unit_id: i64,
        locale: &str,
        candidate_key: &str,
        target_text: &str,
        source_attempt_id: Option<i64>,
        score: Option<f64>,
    ) -> Result<i64>;
}

pub trait RunStore {
    fn begin_run(
        &self,
        repository_id: i64,
        invocation_key: &str,
        config_path: &Path,
        metadata_json: &str,
        policy_fingerprint: &str,
    ) -> Result<String>;
    fn finish_run(&self, run_id: &str, status: &str) -> Result<bool>;
    fn enqueue_work_item(
        &self,
        run_id: &str,
        unit_id: i64,
        locale: &str,
        kind: &str,
        priority: i64,
        input_json: &str,
    ) -> Result<i64>;
    fn enqueue_document_work_item(
        &self,
        run_id: &str,
        document_id: i64,
        locale: &str,
        kind: &str,
        priority: i64,
        input_json: &str,
    ) -> Result<i64>;
    fn finish_document_work(
        &self,
        work_item_id: i64,
        succeeded: bool,
        result_json: &str,
    ) -> Result<()>;
    fn record_finding(&self, input: FindingInput<'_>) -> Result<i64>;
}

pub trait CanonicalStore {
    fn canonical_file(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
    ) -> Result<Option<CanonicalFile>>;
    fn upsert_canonical_file(&self, input: CanonicalFileInput<'_>) -> Result<i64>;
    fn persist_canonical_file(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
    ) -> Result<CanonicalFile>;
    fn canonical_document_intent(&self, content_version_id: i64) -> Result<Option<String>>;
    fn bind_canonical_document_intent(
        &self,
        content_version_id: i64,
        identity_json: &str,
    ) -> Result<()>;
    fn record_canonical_file_translations(
        &self,
        canonical_file_id: i64,
        translations: &[CanonicalTranslationInput<'_>],
        locale: &str,
    ) -> Result<usize>;
    fn transition_canonical_file(
        &self,
        id: i64,
        transition: CanonicalTransition,
        materialized_hash: Option<&str>,
    ) -> Result<()>;
    fn canonical_content_matches(
        &self,
        repository_id: i64,
        locale: &str,
        source_revision: &str,
        binding: &PublicationManifestFile,
        file: &PublicationFile,
    ) -> Result<bool>;
    fn canonical_compatible(&self, content_version_id: i64) -> Result<bool>;
    fn persist_canonical_document(
        &self,
        input: CanonicalFileInput<'_>,
        translations: &[CanonicalTranslationInput<'_>],
        identity_json: &str,
    ) -> Result<CanonicalFile>;
}

pub trait EffectStore {
    fn pending_materializations(
        &self,
        repository_id: i64,
        locale: &str,
    ) -> Result<Vec<OutboxEntry>>;
    fn cancel_materialization(&self, repository_id: i64, id: i64) -> Result<()>;
    fn effect_key(&self, kind: OutboxKind, base: &str) -> Result<String>;
    fn materialization_work(&self, dedupe_key: &str) -> Result<Option<i64>>;
    fn supersede_materializations(
        &self,
        repository_id: i64,
        locale: &str,
        path: &str,
        active_dedupe_key: &str,
    ) -> Result<usize>;
    fn enqueue_materialization(
        &self,
        work_item_id: i64,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64>;
    fn claim_outbox_key(
        &self,
        kind: OutboxKind,
        dedupe_key: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>>;
    fn retry_outbox(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        error: &str,
        available_at: i64,
    ) -> Result<bool>;
    fn complete_outbox(&self, kind: OutboxKind, id: i64, owner: &str) -> Result<bool>;
    fn enqueue_publication(
        &self,
        repository_id: i64,
        run_id: Option<&str>,
        locale: &str,
        dedupe_key: &str,
        payload_json: &str,
    ) -> Result<i64>;
    fn claim_publication_locale(
        &self,
        repository_id: i64,
        locale: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<OutboxEntry>>;
    fn update_outbox_payload(
        &self,
        kind: OutboxKind,
        id: i64,
        owner: &str,
        payload_json: &str,
    ) -> Result<bool>;
    fn schedule_materialization(
        &self,
        input: MaterializationIntentInput<'_>,
    ) -> Result<MaterializationReceipt>;
}

pub trait PublicationStore {
    fn pull_request_for_branch(
        &self,
        repository_id: i64,
        provider: &str,
        branch: &str,
    ) -> Result<Option<StoredPullRequest>>;
    fn record_pr_state(&self, input: PullRequestStateInput<'_>) -> Result<i64>;
    fn record_publication_manifest(&self, input: PublicationManifestInput<'_>) -> Result<i64>;
    fn record_publication_authorization(
        &self,
        input: PublicationManifestInput<'_>,
        authorization_key: &str,
    ) -> Result<i64>;
    fn transition_publication_authorization(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        authorization_key: &str,
        state: PublicationState,
    ) -> Result<()>;
    fn transition_publication_manifest(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        state: PublicationState,
    ) -> Result<()>;
    fn publication_snapshot(
        &self,
        repository_id: i64,
        locale: &str,
        commit: &str,
    ) -> Result<Vec<CanonicalSnapshot>>;
    fn promote_merged_publication(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        provenance: &str,
        verified_zero_unit_contents: &[i64],
    ) -> Result<usize>;
}

/// Aggregate required storage capabilities for the application composition root.
pub trait StateStore:
    DocumentStore + TranslationStore + RunStore + CanonicalStore + EffectStore + PublicationStore
{
}
