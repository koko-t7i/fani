//! Durable protocol types shared by application services and persistence.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentIdentity {
    pub source_path: String,
    pub source_revision: String,
    pub source_hash: String,
    pub source_set_id: String,
    pub mapping_identity: String,
    pub locale: String,
    pub target_path: String,
    pub contract: crate::domain::document::FormatContract,
    pub request_schema: String,
    pub policy_fingerprint: String,
    pub request_identity: String,
}

impl DocumentIdentity {
    pub fn request_hash(&self) -> String {
        let mut request = self.clone();
        request.request_identity.clear();
        content_hash(&serde_json::to_vec(&request).expect("document identity serialization"))
    }

    pub fn mapping_hash(&self) -> String {
        let mut mapping = self.clone();
        mapping.source_revision.clear();
        mapping.request_hash()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MaterializationPayload {
    pub repository_id: i64,
    pub locale: String,
    pub path: String,
    pub document_identity: DocumentIdentity,
    pub content_hash: String,
    pub canonical_content_version_id: i64,
    pub canonical_file_id: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublicationRecord {
    #[serde(default)]
    pub document_identity: Option<DocumentIdentity>,
    pub canonical_content_version_id: i64,
    pub canonical_file_id: i64,
    pub content: String,
    pub content_hash: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublicationPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_remote_tip: Option<Option<String>>,
    pub files: Vec<PublicationRecord>,
    pub language: String,
    pub policy_fingerprint: String,
    pub run_id: String,
    pub source_revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PipelineRequest {
    pub source_revision: String,
    pub path: String,
    pub unit: String,
    pub document_identity: DocumentIdentity,
}

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?)
}
pub fn decode<T: serde::de::DeserializeOwned>(value: &str) -> anyhow::Result<T> {
    Ok(serde_json::from_str(value)?)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MaterializationWorkResult {
    pub status: MaterializationStatus,
    pub content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationStatus {
    Written,
    AlreadyCurrent,
    HumanEdit,
    Rejected,
    Adopted,
    Discarded,
}
impl MaterializationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::AlreadyCurrent => "already_current",
            Self::HumanEdit => "human_edit",
            Self::Rejected => "rejected",
            Self::Adopted => "adopted",
            Self::Discarded => "discarded",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Started,
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}
impl AttemptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
    pub fn from_storage(value: &str) -> anyhow::Result<Self> {
        match value {
            "started" => Ok(Self::Started),
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "timed_out" => Ok(Self::TimedOut),
            other => anyhow::bail!("unsupported durable attempt status: {other}"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum WorkKind {
    Assembly,
    ProjectCheck,
    Pipeline,
}
impl WorkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assembly => "assembly",
            Self::ProjectCheck => "project_check",
            Self::Pipeline => "pipeline",
        }
    }
}

#[derive(Deserialize)]
pub struct StoredReviewRequest {
    pub schema: String,
    pub task: crate::domain::model::AgentTask,
}

#[derive(Serialize)]
pub struct AssemblyRequest<'a> {
    pub source_revision: &'a str,
    pub path: &'a str,
    pub target_path: &'a str,
    pub contract: &'a crate::domain::document::FormatContract,
    pub document_identity: &'a DocumentIdentity,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum DocumentWorkResult<'a> {
    Assembled { content_hash: &'a str },
    Rejected { code: &'a str },
    Checked { manifest_hash: &'a str },
}

#[derive(Serialize)]
pub struct CheckManifestFile<'a> {
    pub path: &'a str,
    pub content_hash: String,
}
#[derive(Serialize)]
pub struct CheckRequest<'a> {
    pub source_revision: &'a str,
    pub manifest: &'a [CheckManifestFile<'a>],
    pub manifest_hash: &'a str,
}
#[derive(Serialize)]
pub struct CheckFailure<'a> {
    pub argv: &'a [String],
    pub command_index: usize,
    pub timed_out: bool,
}

pub fn manifest_hash(manifest: &[CheckManifestFile<'_>]) -> anyhow::Result<String> {
    // Historical hashes used sorted JSON object keys.
    Ok(content_hash(&serde_json::to_vec(&serde_json::to_value(
        manifest,
    )?)?))
}

#[derive(Serialize)]
pub struct DeterministicRepairRequest<'a> {
    pub schema: &'static str,
    pub task_id: &'a str,
    pub operation: &'a str,
    pub source_attempt_id: i64,
    pub source_output_hash: &'a str,
}
#[derive(Serialize)]
pub struct DeterministicRepairResponse<'a> {
    pub schema: &'static str,
    pub task_id: &'a str,
    pub output: &'a str,
}
