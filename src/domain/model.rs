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
    pub repository: String,
    pub source_revision: String,
    pub path: String,
    pub bytes: Vec<u8>,
    pub content_hash: String,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentTask {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LanguageOutcome {
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
    pub repository: PathBuf,
    pub language: String,
    pub source_revision: String,
    pub documents: usize,
    pub pending_units: usize,
    pub reused_units: usize,
    pub conflicts: usize,
    pub deferred_units: usize,
}
