use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const STAGES: [&str; 3] = ["translate", "revision", "proofread"];

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
pub struct AgentConfig {
    #[serde(skip)]
    pub name: String,
    pub cmd: Vec<String>,
    #[serde(default = "default_agent_stages")]
    pub stages: Vec<String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_timeout")]
    pub timeout_s: f64,
    #[serde(default = "default_retries")]
    pub retries: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RepoStages {
    #[serde(default)]
    pub revision: bool,
    #[serde(default)]
    pub proofread: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RepoConfig {
    pub path: PathBuf,
    pub languages: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
    #[serde(default = "default_max_tasks")]
    pub max_tasks: usize,
    #[serde(default = "default_repair_budget")]
    pub repair_budget: usize,
    #[serde(default = "default_full_guard")]
    pub full_retranslate_guard: usize,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default = "default_true")]
    pub commit: bool,
    #[serde(default)]
    pub push: bool,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default)]
    pub stages: RepoStages,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub skill: PathBuf,
    #[serde(rename = "repo")]
    pub repos: Vec<RepoConfig>,
    pub agents: HashMap<String, AgentConfig>,
    #[serde(default)]
    pub routing: HashMap<String, String>,
}

fn default_agent_stages() -> Vec<String> {
    vec!["translate".into()]
}
fn default_concurrency() -> usize {
    6
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
fn default_state_dir() -> String {
    ".claude/i18n".into()
}
fn default_max_tasks() -> usize {
    40
}
fn default_repair_budget() -> usize {
    2
}
fn default_full_guard() -> usize {
    30
}
fn default_branch() -> String {
    "i18n/{lang}".into()
}
fn default_remote() -> String {
    "origin".into()
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

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.is_file() {
            return Err(ConfigError::Missing(path.to_path_buf()));
        }
        let text = fs::read_to_string(path)
            .map_err(|e| ConfigError::Invalid(format!("cannot read {}: {e}", path.display())))?;
        let mut cfg: Config =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))?;
        cfg.skill = expand_home(&cfg.skill);
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
            let state_path = Path::new(&repo.state_dir);
            if state_path.is_absolute()
                || state_path.components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(ConfigError::Invalid(format!(
                    "{}: state_dir must stay inside the repository",
                    repo.path.display()
                )));
            }
            if repo.languages.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{}: languages must not be empty",
                    repo.path.display()
                )));
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
            for stage in &agent.stages {
                if !STAGES.contains(&stage.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: unknown stage {stage:?}"
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
        Ok(cfg)
    }

    pub fn check_environment(&self) -> Result<(), ConfigError> {
        let run_sh = self.skill.join("scripts/run.sh");
        if !run_sh.is_file() {
            return Err(ConfigError::Invalid(format!(
                "skill not found: {} does not exist. `skill` must point at the i18n skill directory that contains scripts/run.sh",
                run_sh.display()
            )));
        }
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
        if let Some(name) = self.routing.get(stage) {
            let agent = self.agents.get(name).ok_or_else(|| {
                ConfigError::Invalid(format!("routing.{stage} points at unknown agent {name:?}"))
            })?;
            if !agent.enabled {
                return Err(ConfigError::Invalid(format!(
                    "routing.{stage} points at disabled agent {name:?}"
                )));
            }
            if !agent.stages.iter().any(|s| s == stage) {
                return Err(ConfigError::Invalid(format!(
                    "agent {name:?} does not list stage {stage:?} in its stages"
                )));
            }
            return Ok(agent);
        }
        self.agents
            .values()
            .find(|a| a.enabled && a.stages.iter().any(|s| s == stage))
            .ok_or_else(|| {
                ConfigError::Invalid(format!("no enabled agent handles stage {stage:?}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn minimal(root: &Path) -> String {
        format!(
            r#"skill = "{}"
[[repo]]
path = "{}"
languages = ["zh-CN"]
[agents.fake]
cmd = ["true"]
stages = ["translate"]
[routing]
translate = "fake"
"#,
            root.join("skill").display(),
            root.join("repo").display()
        )
    }

    #[test]
    fn loads_defaults_and_routing() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("skill/scripts")).unwrap();
        fs::write(tmp.path().join("skill/scripts/run.sh"), "#!/bin/sh\n").unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        fs::write(tmp.path().join("fani.toml"), minimal(tmp.path())).unwrap();
        let cfg = Config::load(&tmp.path().join("fani.toml")).unwrap();
        assert_eq!(cfg.repos[0].max_tasks, 40);
        assert_eq!(cfg.repos[0].repair_budget, 2);
        assert_eq!(cfg.agent_for("translate").unwrap().name, "fake");
        cfg.check_environment().unwrap();
    }

    #[test]
    fn rejects_disabled_route() {
        let tmp = tempdir().unwrap();
        let text = minimal(tmp.path()).replace(
            "stages = [\"translate\"]",
            "stages = [\"translate\"]\nenabled = false",
        );
        fs::write(tmp.path().join("fani.toml"), text).unwrap();
        assert!(
            Config::load(&tmp.path().join("fani.toml"))
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
    }
}
