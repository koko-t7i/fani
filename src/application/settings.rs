use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityConfig {
    #[serde(default = "default_true")]
    pub revision: bool,
    #[serde(default)]
    pub proofread: bool,
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            revision: true,
            proofread: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentationConfig {
    #[serde(default)]
    pub commands: Vec<Vec<String>>,
    #[serde(default = "default_documentation_timeout")]
    pub timeout_s: f64,
}

impl Default for DocumentationConfig {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            timeout_s: default_documentation_timeout(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub repository: String,
    #[serde(default = "default_base")]
    pub base: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub required_checks: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub push: bool,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default = "default_source_ref")]
    pub source_ref: String,
    #[serde(default)]
    pub github: GithubConfig,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            branch: default_branch(),
            push: false,
            remote: default_remote(),
            source_ref: default_source_ref(),
            github: GithubConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub path: PathBuf,
    pub languages: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_target_pattern")]
    pub target_pattern: String,
    #[serde(default = "default_max_tasks")]
    pub max_tasks: usize,
    #[serde(default = "default_repair_budget")]
    pub repair_budget: usize,
    #[serde(default)]
    pub quality: QualityConfig,
    #[serde(default)]
    pub documentation: DocumentationConfig,
    #[serde(default)]
    pub publish: PublishConfig,
}

const fn default_true() -> bool {
    true
}
fn default_data_dir() -> String {
    ".fani".into()
}
fn default_target_pattern() -> String {
    "docs/{lang}/{relpath}".into()
}
fn default_max_tasks() -> usize {
    40
}
fn default_repair_budget() -> usize {
    2
}
fn default_documentation_timeout() -> f64 {
    300.0
}
fn default_branch() -> String {
    "i18n/{lang}".into()
}
fn default_remote() -> String {
    "origin".into()
}
fn default_source_ref() -> String {
    "HEAD".into()
}
fn default_base() -> String {
    "main".into()
}
