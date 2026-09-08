use crate::domain::model::MessageSyntax;
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

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSet {
    pub format: crate::domain::document::DocumentFormat,
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub strip_prefix: Option<String>,
    pub target_pattern: String,
    #[serde(default)]
    pub message_syntax: Option<MessageSyntax>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "RawRepoConfig")]
pub struct RepoConfig {
    // Report destinations belong to the invocation, not the source configuration.
    pub reserved_paths: Vec<PathBuf>,
    pub path: PathBuf,
    pub languages: Vec<String>,
    pub sources: Option<Vec<SourceSet>>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub data_dir: String,
    pub target_pattern: String,
    pub max_tasks: usize,
    pub repair_budget: usize,
    pub quality: QualityConfig,
    pub documentation: DocumentationConfig,
    pub publish: PublishConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepoConfig {
    pub path: PathBuf,
    pub languages: Vec<String>,
    pub sources: Option<Vec<SourceSet>>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default)]
    pub target_pattern: Option<String>,
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

impl TryFrom<RawRepoConfig> for RepoConfig {
    type Error = String;

    fn try_from(raw: RawRepoConfig) -> Result<Self, Self::Error> {
        if let Some(sources) = &raw.sources {
            if sources.is_empty() {
                return Err("sources must not be empty".into());
            }
            if raw.include.is_some() || raw.exclude.is_some() || raw.target_pattern.is_some() {
                return Err(
                    "sources is mutually exclusive with include, exclude and target_pattern".into(),
                );
            }
        }
        Ok(Self {
            reserved_paths: Vec::new(),
            sources: raw.sources,
            path: raw.path,
            languages: raw.languages,
            include: raw.include.unwrap_or_default(),
            exclude: raw.exclude.unwrap_or_default(),
            data_dir: raw.data_dir,
            target_pattern: raw.target_pattern.unwrap_or_else(default_target_pattern),
            max_tasks: raw.max_tasks,
            repair_budget: raw.repair_budget,
            quality: raw.quality,
            documentation: raw.documentation,
            publish: raw.publish,
        })
    }
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
