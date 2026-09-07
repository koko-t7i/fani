use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    NeedsHuman,
    Error,
    Partial,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NeedsHuman => "needs_human",
            Self::Error => "error",
            Self::Partial => "partial",
        }
    }

    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::NeedsHuman => 1,
            Self::Error => 2,
            Self::Partial => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum DecisionCode {
    ConfigInvalid,
    LockBusy,
    SourceChanged,
    HumanEdit,
    AmbiguousMatch,
    AgentExit,
    AgentTimeout,
    AgentInvalid,
    VerificationFailed,
    PublicationConflict,
    PublicationRetry,
}

impl DecisionCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigInvalid => "CFG-INVALID",
            Self::LockBusy => "LOCK-BUSY",
            Self::SourceChanged => "SRC-CHANGED",
            Self::HumanEdit => "HUMAN-EDIT",
            Self::AmbiguousMatch => "MATCH-AMBIGUOUS",
            Self::AgentExit => "AGENT-EXIT",
            Self::AgentTimeout => "AGENT-TIMEOUT",
            Self::AgentInvalid => "AGENT-INVALID",
            Self::VerificationFailed => "VERIFY-FAILED",
            Self::PublicationConflict => "PUBLISH-CONFLICT",
            Self::PublicationRetry => "PUBLISH-RETRY",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceDocument {
    pub source_format: crate::domain::document::DocumentFormat,
    pub source_set_id: String,
    pub mapping_identity: String,
    pub mapped_relpath: String,
    pub target_pattern: String,
    pub message_syntax: Option<MessageSyntax>,
    pub repository: String,
    pub source_revision: String,
    pub path: String,
    pub bytes: Vec<u8>,
    pub content_hash: String,
}

impl SourceDocument {
    pub fn target_path(&self, language: &str) -> PathBuf {
        self.target_pattern
            .replace("{lang}", language)
            .replace("{relpath}", &self.mapped_relpath)
            .into()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TranslationUnit {
    pub id: String,
    pub document_id: String,
    pub ordinal: usize,
    pub kind: String,
    pub range: Range<usize>,
    pub source: String,
    pub source_hash: String,
    pub context_hash: String,
    pub protected_tokens: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnitTranslation {
    pub unit_id: String,
    pub translated: String,
    pub source_hash: String,
    pub context_hash: String,
    pub trusted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub code: String,
    pub path: String,
    pub unit_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Error,
    Warning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Exact,
    SourceChanged,
    StructurallyChanged,
    Orphaned,
}

impl Freshness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::SourceChanged => "source_changed",
            Self::StructurallyChanged => "structurally_changed",
            Self::Orphaned => "orphaned",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationProvenance {
    Human,
    Imported,
    TrustedTm,
    CandidateTm,
    Ai,
    RepairedAi,
}

impl TranslationProvenance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Imported => "imported",
            Self::TrustedTm => "trusted_tm",
            Self::CandidateTm => "candidate_tm",
            Self::Ai => "ai",
            Self::RepairedAi => "repaired_ai",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationState {
    Pending,
    Passed,
    Failed,
    Quarantined,
}

impl ValidationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Quarantined => "quarantined",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Unreviewed,
    NeedsReview,
    Approved,
    Rejected,
}

impl ReviewState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unreviewed => "unreviewed",
            Self::NeedsReview => "needs_review",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Candidate,
    CommitCreated,
    PushPending,
    PrOpen,
    Merged,
    Superseded,
}

impl PublicationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::CommitCreated => "commit_created",
            Self::PushPending => "push_pending",
            Self::PrOpen => "pr_open",
            Self::Merged => "merged",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTier {
    Trusted,
    Candidate,
    History,
}

impl MemoryTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Candidate => "candidate",
            Self::History => "history",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalTransition {
    Materialized,
    HumanEdit,
    Adopted,
    CommitCreated,
    PushPending,
    PrOpen,
    Merged,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub content_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileChange {
    Add { path: String, bytes: Vec<u8> },
    Modify { path: String, bytes: Vec<u8> },
    Delete { path: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AgentStage {
    Translate,
    Repair,
    Revision,
    Proofread,
}

impl AgentStage {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Translate => "translate",
            Self::Repair => "repair",
            Self::Revision => "revision",
            Self::Proofread => "proofread",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum MessageSyntax {
    #[serde(rename = "plain")]
    Plain,
    #[serde(rename = "i18next-interpolation-v1")]
    I18nextInterpolationV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPermissions {
    pub contract: String,
    pub reorderable_tokens: Vec<String>,
}

impl TokenPermissions {
    pub fn for_unit(unit: &crate::domain::document::TranslatableUnit) -> Self {
        Self {
            contract: unit.context.token_contract.clone(),
            reorderable_tokens: unit
                .protected
                .iter()
                .filter(|span| {
                    matches!(
                        span.kind,
                        crate::domain::document::ProtectedKind::InlineCode
                    )
                })
                .map(|span| span.token.clone())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTask {
    pub source_format: crate::domain::document::DocumentFormat,
    pub unit_context: crate::domain::document::UnitContext,
    pub context_key: String,
    pub message_syntax: Option<MessageSyntax>,
    pub token_permissions: TokenPermissions,
    pub id: String,
    pub stage: AgentStage,
    pub source_language: String,
    pub target_language: String,
    pub source: String,
    pub previous_source: Option<String>,
    pub previous_translation: Option<String>,
    pub findings: Vec<Finding>,
    pub protected_tokens: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentResult {
    pub task_id: String,
    pub ok: bool,
    pub output: String,
    pub code: Option<String>,
    pub attempts: usize,
    pub duration_s: f64,
    pub diagnostic: String,
    #[serde(skip)]
    pub request_json: String,
    #[serde(skip)]
    pub response_json: Option<String>,
    #[serde(skip)]
    pub prompt_version: String,
    #[serde(skip)]
    pub prompt_hash: String,
    #[serde(skip)]
    pub policy_fingerprint: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Published {
    pub branch: String,
    pub commit: String,
    pub paths: Vec<String>,
    pub pushed: bool,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub skipped: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DocumentStatistics {
    pub markdown_files: usize,
    pub mdx_files: usize,
    pub json_files: usize,
    pub parse_failures: usize,
    pub verified_documents: usize,
    pub pass_through_documents: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LanguageOutcome {
    #[serde(flatten)]
    pub documents: DocumentStatistics,
    pub repo: String,
    pub lang: String,
    pub source_revision: String,
    pub status: Status,
    pub run_id: String,
    pub message: String,
    pub written: Vec<String>,
    pub conflicts: Vec<Finding>,
    pub findings: Vec<Finding>,
    pub agent_calls: Vec<AgentResult>,
    pub published: Published,
    pub repair_rounds: usize,
    pub remaining_tasks: usize,
    pub reused_units: usize,
    pub duration_s: f64,
    #[serde(skip)]
    pub transitions: Vec<String>,
}

impl LanguageOutcome {
    pub fn new(repo: &std::path::Path, lang: &str) -> Self {
        Self {
            documents: DocumentStatistics::default(),
            repo: repo.display().to_string(),
            lang: lang.to_string(),
            source_revision: String::new(),
            status: Status::Ok,
            run_id: String::new(),
            message: String::new(),
            written: Vec::new(),
            conflicts: Vec::new(),
            findings: Vec::new(),
            agent_calls: Vec::new(),
            published: Published::default(),
            repair_rounds: 0,
            remaining_tasks: 0,
            reused_units: 0,
            duration_s: 0.0,
            transitions: vec!["planning".into()],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlanSummary {
    #[serde(flatten)]
    pub document_statistics: DocumentStatistics,
    pub repository: PathBuf,
    pub language: String,
    pub source_revision: String,
    pub documents: usize,
    pub pending_units: usize,
    pub reused_units: usize,
    pub conflicts: usize,
    pub deferred_units: usize,
}
