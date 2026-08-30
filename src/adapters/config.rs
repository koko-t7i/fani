use crate::application::settings::RepoConfig;
use globset::Glob;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const STAGES: [&str; 4] = ["translate", "repair", "revision", "proofread"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    Missing(PathBuf),
    #[error("{0}: {1}")]
    Parse(PathBuf, toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(skip)]
    pub name: String,
    pub cmd: Vec<String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_timeout")]
    pub timeout_s: f64,
    #[serde(default = "default_retries")]
    pub retries: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub env_allow: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
    pub agents: HashMap<String, AgentConfig>,
    #[serde(default)]
    pub routing: HashMap<String, String>,
}

fn default_concurrency() -> usize {
    4
}
fn default_timeout() -> f64 {
    300.0
}
fn default_retries() -> usize {
    2
}
fn default_true() -> bool {
    true
}

fn expand_home(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(raw.trim_start_matches("~/"));
        }
    }
    path.to_path_buf()
}

fn validate_repo_relative(value: &str, field: &str, repo: &Path) -> Result<(), ConfigError> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ConfigError::Invalid(format!(
            "{}: {field} must be a non-empty path inside the repository",
            repo.display()
        )));
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.is_file() {
            return Err(ConfigError::Missing(path.to_path_buf()));
        }
        let text = fs::read_to_string(path)
            .map_err(|e| ConfigError::Invalid(format!("cannot read {}: {e}", path.display())))?;
        let mut cfg: Config =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))?;
        if cfg.repos.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "{}: at least one [[repo]] table is required",
                path.display()
            )));
        }
        if cfg.agents.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "{}: at least one [agents.<name>] table is required",
                path.display()
            )));
        }
        for repo in &mut cfg.repos {
            repo.path = expand_home(&repo.path);
            validate_repo_relative(&repo.data_dir, "data_dir", &repo.path)?;
            if repo.languages.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{}: languages must not be empty",
                    repo.path.display()
                )));
            }
            if repo.max_tasks == 0 {
                return Err(ConfigError::Invalid(format!(
                    "{}: max_tasks must be greater than zero",
                    repo.path.display()
                )));
            }
            for pattern in repo.include.iter().chain(&repo.exclude) {
                Glob::new(pattern).map_err(|error| {
                    ConfigError::Invalid(format!(
                        "{}: invalid Markdown glob {pattern:?}: {error}",
                        repo.path.display()
                    ))
                })?;
            }
            for token in ["{lang}", "{relpath}"] {
                if !repo.target_pattern.contains(token) {
                    return Err(ConfigError::Invalid(format!(
                        "{}: target_pattern must contain {token}",
                        repo.path.display()
                    )));
                }
            }
            validate_repo_relative(
                &repo
                    .target_pattern
                    .replace("{lang}", "language")
                    .replace("{relpath}", "document.md"),
                "target_pattern",
                &repo.path,
            )?;
            if repo.publish.enabled {
                if !repo.publish.branch.contains("{lang}") {
                    return Err(ConfigError::Invalid(format!(
                        "{}: publish.branch must contain {{lang}}",
                        repo.path.display()
                    )));
                }
                if repo.publish.source_ref.trim().is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "{}: publish.source_ref must not be empty",
                        repo.path.display()
                    )));
                }
            }
            if repo.publish.github.enabled {
                if !repo.publish.enabled || !repo.publish.push {
                    return Err(ConfigError::Invalid(format!(
                        "{}: GitHub publication requires publish.enabled=true and publish.push=true",
                        repo.path.display()
                    )));
                }
                if repo.publish.github.repository.trim().is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "{}: publish.github.repository is required",
                        repo.path.display()
                    )));
                }
            }
        }
        for (name, agent) in &mut cfg.agents {
            agent.name = name.clone();
            if agent.cmd.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "[agents.{name}]: cmd must not be empty"
                )));
            }
            if agent.concurrency == 0 {
                return Err(ConfigError::Invalid(format!(
                    "[agents.{name}]: concurrency must be greater than zero"
                )));
            }
            if !agent.timeout_s.is_finite() || agent.timeout_s <= 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "[agents.{name}]: timeout_s must be a positive finite number"
                )));
            }
            for env in &agent.env_allow {
                if env.is_empty() || env.contains('=') || env.as_bytes().contains(&0) {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: invalid env_allow name {env:?}"
                    )));
                }
            }
        }
        for stage in cfg.routing.keys() {
            if !STAGES.contains(&stage.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "[routing]: unknown stage {stage:?}"
                )));
            }
            cfg.agent_for(stage)?;
        }
        cfg.agent_for("translate")?;
        Ok(cfg)
    }

    pub fn check_environment(&self) -> Result<(), ConfigError> {
        for repo in &self.repos {
            if !repo.path.is_dir() {
                return Err(ConfigError::Invalid(format!(
                    "repo path does not exist: {}",
                    repo.path.display()
                )));
            }
        }
        Ok(())
    }

    pub fn agent_for(&self, stage: &str) -> Result<&AgentConfig, ConfigError> {
        if !STAGES.contains(&stage) {
            return Err(ConfigError::Invalid(format!("unknown stage {stage:?}")));
        }
        let requested = self.routing.get(stage).or_else(|| {
            (stage != "translate")
                .then(|| self.routing.get("translate"))
                .flatten()
        });
        if let Some(name) = requested {
            let agent = self.agents.get(name).ok_or_else(|| {
                ConfigError::Invalid(format!("routing.{stage} points at unknown agent {name:?}"))
            })?;
            if !agent.enabled {
                return Err(ConfigError::Invalid(format!(
                    "routing.{stage} points at disabled agent {name:?}"
                )));
            }
            return Ok(agent);
        }
        self.agents
            .values()
            .find(|agent| agent.enabled)
            .ok_or_else(|| ConfigError::Invalid("no enabled Agent is configured".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn minimal(root: &Path) -> String {
        format!(
            r#"[[repo]]
path = "{}"
languages = ["zh-CN"]
target_pattern = "docs/{{lang}}/{{relpath}}"
[agents.fake]
cmd = ["true"]
[routing]
translate = "fake"
"#,
            root.join("repo").display()
        )
    }

    #[test]
    fn loads_native_defaults_and_routing() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        fs::write(tmp.path().join("fani.toml"), minimal(tmp.path())).unwrap();
        let cfg = Config::load(&tmp.path().join("fani.toml")).unwrap();
        assert_eq!(cfg.repos[0].data_dir, ".fani");
        assert!(cfg.repos[0].quality.revision);
        assert_eq!(cfg.agent_for("repair").unwrap().name, "fake");
        cfg.check_environment().unwrap();
    }

    #[test]
    fn rejects_external_skill_and_unknown_fields() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("fani.toml"),
            format!("skill = '/tmp/i18n'\n{}", minimal(tmp.path())),
        )
        .unwrap();
        let error = Config::load(&tmp.path().join("fani.toml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `skill`"), "{error}");
    }

    #[test]
    fn rejects_target_pattern_without_required_tokens() {
        let tmp = tempdir().unwrap();
        let text = minimal(tmp.path()).replace("docs/{lang}/{relpath}", "docs/{lang}/fixed.md");
        fs::write(tmp.path().join("fani.toml"), text).unwrap();
        assert!(
            Config::load(&tmp.path().join("fani.toml"))
                .unwrap_err()
                .to_string()
                .contains("{relpath}")
        );
    }
}
