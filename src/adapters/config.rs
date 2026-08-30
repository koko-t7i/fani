use crate::application::settings::RepoConfig;
use globset::Glob;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use url::Url;

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
    pub provider: String,
    pub model: String,
    #[serde(default = "default_adapter")]
    pub adapter: String,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
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

fn default_adapter() -> String {
    "native-http-v1".into()
}
fn default_max_output_tokens() -> u32 {
    8192
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

impl AgentConfig {
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref().or(match self.provider.as_str() {
            "anthropic" => Some("https://api.anthropic.com/v1/messages"),
            "openai" => Some("https://api.openai.com/v1/chat/completions"),
            "xai" => Some("https://api.x.ai/v1/chat/completions"),
            "deepseek" => Some("https://api.deepseek.com/chat/completions"),
            _ => None,
        })
    }

    pub fn api_key_env(&self) -> Option<&str> {
        self.api_key_env
            .as_deref()
            .or(match self.provider.as_str() {
                "anthropic" => Some("ANTHROPIC_API_KEY"),
                "openai" => Some("OPENAI_API_KEY"),
                "xai" => Some("XAI_API_KEY"),
                "deepseek" => Some("DEEPSEEK_API_KEY"),
                _ => None,
            })
    }
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
            if !repo.documentation.timeout_s.is_finite() || repo.documentation.timeout_s <= 0.0 {
                return Err(ConfigError::Invalid(format!(
                    "{}: documentation.timeout_s must be a positive finite number",
                    repo.path.display()
                )));
            }
            for (index, command) in repo.documentation.commands.iter().enumerate() {
                if command.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "{}: documentation.commands[{index}] must be a non-empty argv array",
                        repo.path.display()
                    )));
                }
                for argument in command {
                    if argument.is_empty() || argument.as_bytes().contains(&0) {
                        return Err(ConfigError::Invalid(format!(
                            "{}: documentation.commands[{index}] contains an invalid argv value",
                            repo.path.display()
                        )));
                    }
                }
            }
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
            for (field, value) in [
                ("provider", agent.provider.as_str()),
                ("model", agent.model.as_str()),
                ("adapter", agent.adapter.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: {field} must not be empty"
                    )));
                }
            }
            match agent.adapter.as_str() {
                "command-json-v1" => {
                    if agent.cmd.is_empty() {
                        return Err(ConfigError::Invalid(format!(
                            "[agents.{name}]: cmd must not be empty for command-json-v1"
                        )));
                    }
                }
                "native-http-v1" => {
                    if !agent.cmd.is_empty() {
                        return Err(ConfigError::Invalid(format!(
                            "[agents.{name}]: cmd is only valid with command-json-v1"
                        )));
                    }
                    if !matches!(
                        agent.provider.as_str(),
                        "anthropic" | "openai" | "xai" | "deepseek" | "openai-compatible"
                    ) {
                        return Err(ConfigError::Invalid(format!(
                            "[agents.{name}]: native-http-v1 does not support provider {:?}",
                            agent.provider
                        )));
                    }
                    if agent.provider == "openai-compatible" {
                        if agent.endpoint.is_none() {
                            return Err(ConfigError::Invalid(format!(
                                "[agents.{name}]: openai-compatible requires endpoint"
                            )));
                        }
                        if agent.api_key_env.is_none() {
                            return Err(ConfigError::Invalid(format!(
                                "[agents.{name}]: openai-compatible requires api_key_env"
                            )));
                        }
                    } else if agent.endpoint.is_some() || agent.api_key_env.is_some() {
                        return Err(ConfigError::Invalid(format!(
                            "[agents.{name}]: official providers use fixed endpoint and credential names; use openai-compatible for overrides"
                        )));
                    }
                }
                _ => {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: unsupported adapter {:?}",
                        agent.adapter
                    )));
                }
            }
            if let Some(endpoint) = &agent.endpoint {
                let url = Url::parse(endpoint).map_err(|_| {
                    ConfigError::Invalid(format!("[agents.{name}]: endpoint is not a valid URL"))
                })?;
                if !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: endpoint must not contain userinfo or a fragment"
                    )));
                }
                let loopback = url
                    .host_str()
                    .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                    .is_some_and(|address| address.is_loopback());
                if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: endpoint must use HTTPS (HTTP is allowed only for loopback testing)"
                    )));
                }
            }
            if let Some(name) = &agent.api_key_env {
                if name.is_empty() || name.contains('=') || name.as_bytes().contains(&0) {
                    return Err(ConfigError::Invalid(format!(
                        "[agents.{name}]: invalid api_key_env name {name:?}"
                    )));
                }
            }
            if agent.max_output_tokens == 0 {
                return Err(ConfigError::Invalid(format!(
                    "[agents.{name}]: max_output_tokens must be greater than zero"
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
provider = "fixture"
model = "fixture"
adapter = "command-json-v1"
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
    fn rejects_missing_identity_and_unknown_agent_fields() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let config = tmp.path().join("fani.toml");
        fs::write(
            &config,
            minimal(tmp.path()).replace("provider = \"fixture\"\n", ""),
        )
        .unwrap();
        assert!(
            Config::load(&config)
                .unwrap_err()
                .to_string()
                .contains("missing field `provider`")
        );

        fs::write(
            &config,
            minimal(tmp.path()).replace(
                "adapter = \"command-json-v1\"",
                "adapter = \"command-json-v1\"\nlegacy_output = true",
            ),
        )
        .unwrap();
        assert!(
            Config::load(&config)
                .unwrap_err()
                .to_string()
                .contains("unknown field `legacy_output`")
        );
    }

    #[test]
    fn built_in_provider_needs_only_provider_and_model() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let text = minimal(tmp.path())
            .replace("provider = \"fixture\"", "provider = \"anthropic\"")
            .replace("model = \"fixture\"", "model = \"claude-test\"")
            .replace("adapter = \"command-json-v1\"\ncmd = [\"true\"]\n", "");
        fs::write(tmp.path().join("fani.toml"), text).unwrap();
        let config = Config::load(&tmp.path().join("fani.toml")).unwrap();
        let agent = config.agent_for("translate").unwrap();
        assert_eq!(agent.adapter, "native-http-v1");
        assert_eq!(
            agent.endpoint(),
            Some("https://api.anthropic.com/v1/messages")
        );
        assert_eq!(agent.api_key_env(), Some("ANTHROPIC_API_KEY"));
        assert!(agent.cmd.is_empty());
    }

    #[test]
    fn rejects_unknown_built_in_provider_without_custom_command() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let text = minimal(tmp.path())
            .replace("provider = \"fixture\"", "provider = \"unknown\"")
            .replace("adapter = \"command-json-v1\"\ncmd = [\"true\"]\n", "");
        fs::write(tmp.path().join("fani.toml"), text).unwrap();
        let error = Config::load(&tmp.path().join("fani.toml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not support provider"), "{error}");
    }

    #[test]
    fn openai_compatible_requires_key_name_and_secure_endpoint() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let base = minimal(tmp.path())
            .replace("provider = \"fixture\"", "provider = \"openai-compatible\"")
            .replace("adapter = \"command-json-v1\"\ncmd = [\"true\"]\n", "");
        let path = tmp.path().join("fani.toml");

        fs::write(
            &path,
            base.replace(
                "model = \"fixture\"",
                "model = \"fixture\"\nendpoint = \"https://models.example.com/v1/chat/completions\"",
            ),
        )
        .unwrap();
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("requires api_key_env"), "{error}");

        fs::write(
            &path,
            base.replace(
                "model = \"fixture\"",
                "model = \"fixture\"\nendpoint = \"http://models.example.com/v1/chat/completions\"\napi_key_env = \"MODEL_API_KEY\"",
            ),
        )
        .unwrap();
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("must use HTTPS"), "{error}");

        fs::write(
            &path,
            base.replace(
                "model = \"fixture\"",
                "model = \"fixture\"\nendpoint = \"http://localhost:123@remote.example/v1/chat/completions\"\napi_key_env = \"MODEL_API_KEY\"",
            ),
        )
        .unwrap();
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("userinfo"), "{error}");
    }

    #[test]
    fn official_provider_rejects_endpoint_and_key_overrides() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("repo")).unwrap();
        let text = minimal(tmp.path())
            .replace("provider = \"fixture\"", "provider = \"anthropic\"")
            .replace("adapter = \"command-json-v1\"\ncmd = [\"true\"]", "")
            .replace(
                "model = \"fixture\"",
                "model = \"fixture\"\nendpoint = \"https://attacker.example/v1/messages\"",
            );
        let path = tmp.path().join("fani.toml");
        fs::write(&path, text).unwrap();
        let error = Config::load(&path).unwrap_err().to_string();
        assert!(error.contains("fixed endpoint"), "{error}");
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
