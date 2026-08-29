use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    NeedsHuman,
    Partial,
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NeedsHuman => "needs_human",
            Self::Partial => "partial",
            Self::Error => "error",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::NeedsHuman => 1,
            Self::Error => 2,
            Self::Partial => 3,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Published {
    pub branch: String,
    pub commit: String,
    pub paths: Vec<String>,
    pub pushed: bool,
    pub skipped: String,
    #[serde(default)]
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub task_id: String,
    pub ok: bool,
    pub code: Option<String>,
    pub attempts: usize,
    pub duration_s: f64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LangOutcome {
    pub repo: String,
    pub lang: String,
    pub status: Status,
    pub run_id: String,
    pub message: String,
    pub written: Vec<Value>,
    pub conflicts: Vec<Value>,
    pub findings: Vec<Value>,
    pub dispatch: Vec<TaskOutcome>,
    pub published: Published,
    pub repair_rounds: usize,
    pub remaining_tasks: usize,
    pub fuzzy_matched: usize,
    pub duration_s: f64,
    #[serde(skip)]
    pub transitions: Vec<String>,
}

impl LangOutcome {
    pub fn new(repo: &std::path::Path, lang: &str) -> Self {
        Self {
            repo: repo.display().to_string(),
            lang: lang.to_string(),
            status: Status::Ok,
            run_id: String::new(),
            message: String::new(),
            written: vec![],
            conflicts: vec![],
            findings: vec![],
            dispatch: vec![],
            published: Published::default(),
            repair_rounds: 0,
            remaining_tasks: 0,
            fuzzy_matched: 0,
            duration_s: 0.0,
            transitions: vec!["planning".into()],
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PlanResult {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub task_count: usize,
    #[serde(default)]
    pub conflicts: Vec<Value>,
    #[serde(default)]
    pub files: Vec<Value>,
    #[serde(default)]
    pub fuzzy_matched: usize,
    #[serde(default)]
    pub truncated_tasks: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ApplyResult {
    #[serde(default)]
    pub written: Vec<Value>,
    #[serde(default)]
    pub rejected: Vec<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VerifyResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub findings: Vec<Value>,
    #[serde(default)]
    pub retry_files: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReviewPlanResult {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub task_count: usize,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReviewCollectResult {
    #[serde(default)]
    pub findings: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Task {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub chunk_id: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub source: String,
    pub result_path: PathBuf,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub previous_source: String,
    #[serde(default)]
    pub previous_translation: String,
    #[serde(default)]
    pub match_ratio: Value,
}
