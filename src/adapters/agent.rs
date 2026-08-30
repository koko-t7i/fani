use crate::adapters::config::{AgentConfig, Config};
use crate::adapters::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use crate::application::ports::{
    AGENT_REQUEST_SCHEMA, AGENT_RESPONSE_SCHEMA, AgentExecution, AgentExecutor, AgentPolicy,
    AgentPrompt, AgentRequestEnvelope, AgentResponseEnvelope,
};
use crate::domain::model::{AgentResult, AgentTask, DecisionCode};
use crate::domain::prompts;
use anyhow::{Context, Result, anyhow};
use nix::unistd::{Pid, setpgid};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wait_timeout::ChildExt;

const OUTPUT_FILE_TOKEN: &str = "{output_file}";
const MAX_STDIN_BYTES: usize = 4 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

type ReadResult = (&'static str, std::io::Result<(Vec<u8>, bool)>);

#[derive(Debug)]
struct NativeFailure {
    code: String,
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl NativeFailure {
    fn permanent(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }

    fn transient(code: &str, message: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: true,
            retry_after,
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    name: &'static str,
    mut reader: R,
    limit: usize,
    tail: bool,
    sender: mpsc::Sender<ReadResult>,
) {
    thread::spawn(move || {
        let result = (|| {
            let mut output = Vec::new();
            let mut truncated = false;
            let mut chunk = [0_u8; 8192];
            loop {
                let count = reader.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                if tail {
                    let excess = output.len().saturating_add(count).saturating_sub(limit);
                    if excess > 0 {
                        output.drain(..excess.min(output.len()));
                        truncated = true;
                    }
                    if count > limit {
                        output.clear();
                        output.extend_from_slice(&chunk[count - limit..count]);
                        truncated = true;
                    } else {
                        output.extend_from_slice(&chunk[..count]);
                    }
                } else {
                    let remaining = limit.saturating_sub(output.len());
                    let keep = remaining.min(count);
                    output.extend_from_slice(&chunk[..keep]);
                    truncated |= keep != count;
                }
            }
            Ok((output, truncated))
        })();
        let _ = sender.send((name, result));
    });
}

fn isolated_environment(command: &mut std::process::Command, config: &AgentConfig, home: &Path) {
    let path = std::env::var_os("PATH");
    command.env_clear().env("HOME", home).env("LANG", "C.UTF-8");
    if let Some(path) = path {
        command.env("PATH", path);
    }
    for name in &config.env_allow {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

fn read_output_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_ANSWER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ANSWER_BYTES {
        return Err(anyhow!("Agent result exceeded {MAX_ANSWER_BYTES} bytes"));
    }
    String::from_utf8(bytes).context("Agent result is not UTF-8")
}

fn execute_once(
    config: &AgentConfig,
    task_id: &str,
    request_json: &str,
) -> Result<(String, String, String, f64), (String, String)> {
    let started = Instant::now();
    if request_json.len() > MAX_STDIN_BYTES {
        return Err((
            DecisionCode::AgentInvalid.as_str().into(),
            "Agent JSON request exceeds input limit".into(),
        ));
    }
    enable_subreaper()
        .map_err(|error| (DecisionCode::AgentExit.as_str().into(), error.to_string()))?;
    let sandbox = TempDir::new()
        .map_err(|error| (DecisionCode::AgentExit.as_str().into(), error.to_string()))?;
    let home = sandbox.path().join("home");
    std::fs::create_dir(&home)
        .map_err(|error| (DecisionCode::AgentExit.as_str().into(), error.to_string()))?;
    let output_path = sandbox.path().join("result.txt");
    let cmdline: Vec<String> = config
        .cmd
        .iter()
        .map(|part| part.replace(OUTPUT_FILE_TOKEN, &output_path.display().to_string()))
        .collect();
    let Some(program) = cmdline.first() else {
        return Err((
            DecisionCode::AgentExit.as_str().into(),
            "empty Agent command".into(),
        ));
    };
    let token = process_token();
    let mut command = wrapped_command(program)
        .map_err(|error| (DecisionCode::AgentExit.as_str().into(), error.to_string()))?;
    command
        .args(&cmdline[1..])
        .current_dir(sandbox.path())
        .env("FANI_PROCESS_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolated_environment(&mut command, config, &home);
    command.env("FANI_PROCESS_TOKEN", &token);
    unsafe {
        command.pre_exec(|| {
            setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::other)
        });
    }
    let (mut child, tracker) = spawn_tracked(&mut command, token)
        .map_err(|error| (DecisionCode::AgentExit.as_str().into(), error.to_string()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = request_json.as_bytes().to_vec();
    let (stdin_tx, stdin_rx) = mpsc::channel();
    thread::spawn(move || {
        let result = stdin.write_all(&input);
        drop(stdin);
        let _ = stdin_tx.send(result);
    });
    let (read_tx, read_rx) = mpsc::channel();
    spawn_reader(
        "stdout",
        child.stdout.take().expect("piped stdout"),
        MAX_ANSWER_BYTES,
        false,
        read_tx.clone(),
    );
    spawn_reader(
        "stderr",
        child.stderr.take().expect("piped stderr"),
        MAX_DIAGNOSTIC_BYTES,
        true,
        read_tx,
    );
    let timeout = Duration::from_secs_f64(config.timeout_s);
    let deadline = started + timeout;
    let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate_process_group(child, deadline, tracker);
            return Err((
                DecisionCode::AgentTimeout.as_str().into(),
                format!("timed out after {:.3}s", config.timeout_s),
            ));
        }
        Err(error) => {
            terminate_process_group(child, deadline, tracker);
            return Err((DecisionCode::AgentExit.as_str().into(), error.to_string()));
        }
    };
    match stdin_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            terminate_process_group(child, deadline, tracker);
            return Err((
                DecisionCode::AgentExit.as_str().into(),
                format!("cannot write Agent stdin: {error}"),
            ));
        }
        Err(_) => {
            terminate_process_group(child, deadline, tracker);
            return Err((
                DecisionCode::AgentTimeout.as_str().into(),
                "Agent stdin did not complete before deadline".into(),
            ));
        }
    }
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        match read_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(("stdout", result)) => {
                stdout = Some(result.map_err(|error| {
                    (DecisionCode::AgentExit.as_str().into(), error.to_string())
                })?)
            }
            Ok(("stderr", result)) => {
                stderr = Some(result.map_err(|error| {
                    (DecisionCode::AgentExit.as_str().into(), error.to_string())
                })?)
            }
            Ok((name, _)) => {
                return Err((
                    DecisionCode::AgentExit.as_str().into(),
                    format!("unknown stream {name}"),
                ));
            }
            Err(RecvTimeoutError::Timeout) => {
                terminate_process_group(child, deadline, tracker);
                return Err((
                    DecisionCode::AgentTimeout.as_str().into(),
                    "Agent output did not drain before deadline".into(),
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err((
                    DecisionCode::AgentExit.as_str().into(),
                    "Agent output readers disconnected".into(),
                ));
            }
        }
    }
    finish_process_group(tracker);
    let (stdout, stdout_truncated) = stdout.expect("stdout collected");
    let (stderr, stderr_truncated) = stderr.expect("stderr collected");
    let diagnostic = if stderr.is_empty() {
        String::new()
    } else if stderr_truncated {
        format!(
            "Agent stderr captured (at least {} bytes; content redacted)",
            stderr.len()
        )
    } else {
        format!(
            "Agent stderr captured ({} bytes; content redacted)",
            stderr.len()
        )
    };
    if !status.success() {
        let suffix = if diagnostic.is_empty() {
            String::new()
        } else {
            format!("; {diagnostic}")
        };
        return Err((
            DecisionCode::AgentExit.as_str().into(),
            format!("exit {}{suffix}", status.code().unwrap_or(-1)),
        ));
    }
    let raw = if config
        .cmd
        .iter()
        .any(|part| part.contains(OUTPUT_FILE_TOKEN))
    {
        read_output_file(&output_path).map_err(|error| {
            (
                DecisionCode::AgentInvalid.as_str().into(),
                error.to_string(),
            )
        })?
    } else {
        if stdout_truncated {
            return Err((
                DecisionCode::AgentInvalid.as_str().into(),
                format!("Agent stdout exceeded {MAX_ANSWER_BYTES} bytes"),
            ));
        }
        String::from_utf8(stdout).map_err(|error| {
            (
                DecisionCode::AgentInvalid.as_str().into(),
                error.to_string(),
            )
        })?
    };
    let response: AgentResponseEnvelope = serde_json::from_str(&raw).map_err(|error| {
        (
            DecisionCode::AgentInvalid.as_str().into(),
            format!("Agent returned invalid JSON envelope: {error}"),
        )
    })?;
    if response.schema != AGENT_RESPONSE_SCHEMA {
        return Err((
            DecisionCode::AgentInvalid.as_str().into(),
            "Agent response uses an unsupported schema".into(),
        ));
    }
    if response.task_id != task_id {
        return Err((
            DecisionCode::AgentInvalid.as_str().into(),
            "Agent response task_id does not match the request".into(),
        ));
    }
    if response.output.is_empty() {
        return Err((
            DecisionCode::AgentInvalid.as_str().into(),
            "Agent response output is empty".into(),
        ));
    }
    let response_json = serde_json::to_string(&response).map_err(|error| {
        (
            DecisionCode::AgentInvalid.as_str().into(),
            format!("cannot serialize Agent response envelope: {error}"),
        )
    })?;
    Ok((
        response.output,
        response_json,
        diagnostic,
        started.elapsed().as_secs_f64(),
    ))
}

fn agent_request(task: &AgentTask) -> (AgentRequestEnvelope, String, String, String) {
    let prompt_version = prompts::PROMPT_VERSION.to_owned();
    let prompt_hash = prompts::task_prompt_hash(task);
    let policy_fingerprint = prompts::policy_fingerprint();
    let request = AgentRequestEnvelope {
        schema: AGENT_REQUEST_SCHEMA.to_owned(),
        task: task.clone(),
        prompt: AgentPrompt {
            version: prompt_version.clone(),
            resource: prompts::resource_name(task).to_owned(),
            hash: prompt_hash.clone(),
            content: prompts::render(task),
        },
        policy: AgentPolicy {
            fingerprint: policy_fingerprint.clone(),
        },
    };
    (request, prompt_version, prompt_hash, policy_fingerprint)
}

fn native_http_once(
    config: &AgentConfig,
    agent: &ureq::Agent,
    task_id: &str,
    prompt: &str,
) -> Result<(String, String, f64), NativeFailure> {
    let started = Instant::now();
    if prompt.len() > MAX_STDIN_BYTES {
        return Err(NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "Agent prompt exceeds input limit",
        ));
    }
    let endpoint = config.endpoint().ok_or_else(|| {
        NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "native provider endpoint is not configured",
        )
    })?;
    let key_name = config.api_key_env().ok_or_else(|| {
        NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "native provider api_key_env is not configured",
        )
    })?;
    let api_key = std::env::var(key_name).map_err(|_| {
        NativeFailure::permanent(
            DecisionCode::AgentExit.as_str(),
            format!("required provider credential {key_name} is not set"),
        )
    })?;
    let mut request = agent.post(endpoint).set("content-type", "application/json");
    let body = if config.provider == "anthropic" {
        request = request
            .set("x-api-key", &api_key)
            .set("anthropic-version", "2023-06-01");
        serde_json::json!({
            "model": config.model,
            "max_tokens": config.max_output_tokens,
            "messages": [{"role": "user", "content": prompt}]
        })
    } else {
        request = request.set("authorization", &format!("Bearer {api_key}"));
        let mut body = serde_json::json!({
            "model": config.model,
            "messages": [{"role": "user", "content": prompt}]
        });
        let token_field = if config.provider == "openai" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[token_field] = serde_json::json!(config.max_output_tokens);
        body
    };
    let body = serde_json::to_vec(&body).map_err(|_| {
        NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "cannot serialize provider request",
        )
    })?;
    if body.len() > MAX_STDIN_BYTES {
        return Err(NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "serialized provider request exceeds input limit",
        ));
    }
    let response = request.send_bytes(&body).map_err(|error| match error {
        ureq::Error::Status(status, response) => {
            let retryable = status == 408 || status == 429 || (500..=599).contains(&status);
            let retry_after = response
                .header("retry-after")
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs);
            let message = format!("provider returned HTTP {status}; response body redacted");
            if retryable {
                NativeFailure::transient(DecisionCode::AgentExit.as_str(), message, retry_after)
            } else {
                NativeFailure::permanent(DecisionCode::AgentExit.as_str(), message)
            }
        }
        ureq::Error::Transport(_) => NativeFailure::transient(
            DecisionCode::AgentExit.as_str(),
            "provider request failed; transport details redacted",
            None,
        ),
    })?;
    let mut response_bytes = Vec::new();
    response
        .into_reader()
        .take((MAX_PROVIDER_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response_bytes)
        .map_err(|_| {
            NativeFailure::transient(
                DecisionCode::AgentExit.as_str(),
                "cannot read provider response; details redacted",
                None,
            )
        })?;
    if response_bytes.len() > MAX_PROVIDER_RESPONSE_BYTES {
        return Err(NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            format!("provider response exceeded {MAX_PROVIDER_RESPONSE_BYTES} bytes"),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&response_bytes).map_err(|_| {
        NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "provider returned invalid JSON; response body redacted",
        )
    })?;
    let output = if config.provider == "anthropic" {
        value
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| {
                        (part.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                            .then(|| part.get("text").and_then(serde_json::Value::as_str))
                            .flatten()
                    })
                    .collect::<String>()
            })
    } else {
        value
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    }
    .filter(|output| !output.is_empty())
    .ok_or_else(|| {
        NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            "provider response does not contain non-empty text output",
        )
    })?;
    if output.len() > MAX_ANSWER_BYTES {
        return Err(NativeFailure::permanent(
            DecisionCode::AgentInvalid.as_str(),
            format!("Agent result exceeded {MAX_ANSWER_BYTES} bytes"),
        ));
    }
    let envelope = AgentResponseEnvelope {
        schema: AGENT_RESPONSE_SCHEMA.to_owned(),
        task_id: task_id.to_owned(),
        output: output.clone(),
    };
    Ok((
        output,
        serde_json::to_string(&envelope).expect("Agent response is serializable"),
        started.elapsed().as_secs_f64(),
    ))
}

#[derive(Clone)]
pub struct CommandAgent {
    config: AgentConfig,
}

impl CommandAgent {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }

    fn execute_one(&self, task: &AgentTask) -> AgentResult {
        let (request, prompt_version, prompt_hash, policy_fingerprint) = agent_request(task);
        let request_json = serde_json::to_string(&request).expect("Agent request is serializable");
        let started = Instant::now();
        let mut last_code = DecisionCode::AgentExit.as_str().to_owned();
        let mut last_message = String::new();
        for attempt in 1..=self.config.retries + 1 {
            match execute_once(&self.config, &task.id, &request_json) {
                Ok((output, response_json, diagnostic, duration_s)) => {
                    return AgentResult {
                        task_id: task.id.clone(),
                        ok: true,
                        output,
                        code: None,
                        attempts: attempt,
                        duration_s,
                        diagnostic,
                        request_json,
                        response_json: Some(response_json),
                        prompt_version,
                        prompt_hash,
                        policy_fingerprint,
                    };
                }
                Err((code, message)) => {
                    last_code = code;
                    last_message = message;
                }
            }
        }
        AgentResult {
            task_id: task.id.clone(),
            ok: false,
            output: String::new(),
            code: Some(last_code),
            attempts: self.config.retries + 1,
            duration_s: started.elapsed().as_secs_f64(),
            diagnostic: last_message,
            request_json,
            response_json: None,
            prompt_version,
            prompt_hash,
            policy_fingerprint,
        }
    }
}

impl CommandAgent {
    fn execute(&self, tasks: &[AgentTask]) -> Result<Vec<AgentResult>> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        let tasks = Arc::new(tasks.to_vec());
        let next = Arc::new(AtomicUsize::new(0));
        let output = Arc::new(Mutex::new(Vec::new()));
        let workers = self.config.concurrency.min(tasks.len()).max(1);
        thread::scope(|scope| {
            for _ in 0..workers {
                let tasks = Arc::clone(&tasks);
                let next = Arc::clone(&next);
                let output = Arc::clone(&output);
                let executor = self.clone();
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= tasks.len() {
                            break;
                        }
                        output
                            .lock()
                            .expect("Agent output lock")
                            .push((index, executor.execute_one(&tasks[index])));
                    }
                });
            }
        });
        let mut output = output.lock().expect("Agent output lock").clone();
        output.sort_by_key(|(index, _)| *index);
        Ok(output.into_iter().map(|(_, result)| result).collect())
    }
}

#[derive(Clone)]
pub struct NativeHttpAgent {
    config: AgentConfig,
    agent: ureq::Agent,
}

impl NativeHttpAgent {
    pub fn new(config: &AgentConfig) -> Self {
        Self {
            config: config.clone(),
            agent: ureq::AgentBuilder::new()
                .try_proxy_from_env(false)
                .redirects(0)
                .timeout(Duration::from_secs_f64(config.timeout_s))
                .build(),
        }
    }

    fn execute_one(&self, task: &AgentTask) -> AgentResult {
        let (request, prompt_version, prompt_hash, policy_fingerprint) = agent_request(task);
        let request_json = serde_json::to_string(&request).expect("Agent request is serializable");
        let started = Instant::now();
        let mut last_code = DecisionCode::AgentExit.as_str().to_owned();
        let mut last_message = String::new();
        let mut attempts = 0;
        for attempt in 1..=self.config.retries + 1 {
            attempts = attempt;
            match native_http_once(&self.config, &self.agent, &task.id, &request.prompt.content) {
                Ok((output, response_json, duration_s)) => {
                    return AgentResult {
                        task_id: task.id.clone(),
                        ok: true,
                        output,
                        code: None,
                        attempts: attempt,
                        duration_s,
                        diagnostic: String::new(),
                        request_json,
                        response_json: Some(response_json),
                        prompt_version,
                        prompt_hash,
                        policy_fingerprint,
                    };
                }
                Err(failure) => {
                    last_code = failure.code;
                    last_message = failure.message;
                    if !failure.retryable || attempt > self.config.retries {
                        break;
                    }
                    let exponential_ms = 200_u64.saturating_mul(1_u64 << (attempt - 1).min(4));
                    thread::sleep(
                        failure
                            .retry_after
                            .unwrap_or_else(|| Duration::from_millis(exponential_ms))
                            .min(Duration::from_secs(30)),
                    );
                }
            }
        }
        AgentResult {
            task_id: task.id.clone(),
            ok: false,
            output: String::new(),
            code: Some(last_code),
            attempts,
            duration_s: started.elapsed().as_secs_f64(),
            diagnostic: last_message,
            request_json,
            response_json: None,
            prompt_version,
            prompt_hash,
            policy_fingerprint,
        }
    }

    fn execute(&self, tasks: &[AgentTask]) -> Result<Vec<AgentResult>> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        let tasks = Arc::new(tasks.to_vec());
        let next = Arc::new(AtomicUsize::new(0));
        let output = Arc::new(Mutex::new(Vec::new()));
        let workers = self.config.concurrency.min(tasks.len()).max(1);
        thread::scope(|scope| {
            for _ in 0..workers {
                let tasks = Arc::clone(&tasks);
                let next = Arc::clone(&next);
                let output = Arc::clone(&output);
                let executor = self.clone();
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= tasks.len() {
                            break;
                        }
                        output
                            .lock()
                            .expect("Agent output lock")
                            .push((index, executor.execute_one(&tasks[index])));
                    }
                });
            }
        });
        let mut output = output.lock().expect("Agent output lock").clone();
        output.sort_by_key(|(index, _)| *index);
        Ok(output.into_iter().map(|(_, result)| result).collect())
    }
}

pub struct RoutedAgentExecutor<'a> {
    config: &'a Config,
}

impl<'a> RoutedAgentExecutor<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    fn config_for_tasks(&self, tasks: &[AgentTask]) -> Result<&AgentConfig> {
        let stage = tasks
            .first()
            .map(|task| &task.stage)
            .ok_or_else(|| anyhow!("cannot route an empty Agent task batch"))?;
        if tasks.iter().any(|task| task.stage != *stage) {
            return Err(anyhow!("Agent task batch contains multiple stages"));
        }
        self.config.agent_for(stage.as_str()).map_err(Into::into)
    }
}

impl AgentExecutor for RoutedAgentExecutor<'_> {
    fn execute(&self, tasks: &[AgentTask]) -> Result<AgentExecution> {
        let config = self.config_for_tasks(tasks)?;
        let stage = tasks[0].stage.as_str();
        let started = Instant::now();
        let agent_id = crate::diagnostics::safe_id(&config.name);
        let provider_id = crate::diagnostics::safe_id(&config.provider);
        let model_id = crate::diagnostics::safe_id(&config.model);
        tracing::info!(
            event = "provider.batch.started",
            stage,
            agent_id,
            provider_id,
            model_id,
            adapter = config.adapter,
            task_count = tasks.len(),
        );
        let mut identity = Sha256::new();
        for value in [&config.provider, &config.model, &config.adapter] {
            identity.update((value.len() as u64).to_be_bytes());
            identity.update(value.as_bytes());
        }
        if config.adapter == "native-http-v1" {
            if let Some(endpoint) = config.endpoint() {
                identity.update((endpoint.len() as u64).to_be_bytes());
                identity.update(endpoint.as_bytes());
            }
            identity.update(config.max_output_tokens.to_be_bytes());
        }
        for argument in &config.cmd {
            identity.update((argument.len() as u64).to_be_bytes());
            identity.update(argument.as_bytes());
        }
        let provider_fingerprint = format!("{:x}", identity.finalize());
        let results = match config.adapter.as_str() {
            "native-http-v1" => NativeHttpAgent::new(config).execute(tasks)?,
            "command-json-v1" => CommandAgent::new(config).execute(tasks)?,
            adapter => return Err(anyhow!("unsupported Agent adapter {adapter:?}")),
        };
        for result in &results {
            tracing::info!(
                event = "provider.task.completed",
                stage,
                task_id = %crate::diagnostics::safe_id(&result.task_id),
                status = if result.ok { "succeeded" } else { "failed" },
                code = result.code.as_deref().unwrap_or("ok"),
                attempts = result.attempts,
                duration_ms = (result.duration_s * 1000.0) as u64,
                prompt_id = %crate::diagnostics::safe_id(&result.prompt_hash),
            );
        }
        tracing::info!(
            event = "provider.batch.completed",
            stage,
            agent_id,
            provider_id,
            model_id,
            adapter = config.adapter,
            provider_fingerprint = %crate::diagnostics::safe_id(&provider_fingerprint),
            status = if results.iter().all(|result| result.ok) {
                "succeeded"
            } else {
                "failed"
            },
            task_count = results.len(),
            duration_ms = started.elapsed().as_millis() as u64,
        );
        Ok(AgentExecution {
            agent: config.name.clone(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            adapter: config.adapter.clone(),
            provider_fingerprint,
            results,
        })
    }
}

pub fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn config(provider: &str, endpoint: String, key_env: &str) -> AgentConfig {
        AgentConfig {
            name: "native".into(),
            provider: provider.into(),
            model: "test-model".into(),
            adapter: "native-http-v1".into(),
            cmd: Vec::new(),
            endpoint: Some(endpoint),
            api_key_env: Some(key_env.into()),
            max_output_tokens: 512,
            concurrency: 1,
            timeout_s: 5.0,
            retries: 0,
            enabled: true,
            env_allow: Vec::new(),
        }
    }

    fn http_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .try_proxy_from_env(false)
            .redirects(0)
            .timeout(Duration::from_secs(5))
            .build()
    }

    fn server(status: &str, body: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap()
                        + 4;
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= headers_end + content_length {
                        break;
                    }
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn native_anthropic_provider_needs_no_external_adapter() {
        let (endpoint, request) =
            server("200 OK", r#"{"content":[{"type":"text","text":"你好"}]}"#);
        let key_env = "FANI_TEST_ANTHROPIC_KEY";
        unsafe { std::env::set_var(key_env, "secret-anthropic-key") };
        let (output, envelope, _) = native_http_once(
            &config("anthropic", endpoint, key_env),
            &http_agent(),
            "task-1",
            "Translate",
        )
        .unwrap();
        unsafe { std::env::remove_var(key_env) };
        assert_eq!(output, "你好");
        assert_eq!(
            serde_json::from_str::<AgentResponseEnvelope>(&envelope)
                .unwrap()
                .task_id,
            "task-1"
        );
        let request = request.join().unwrap();
        assert!(request.contains("x-api-key: secret-anthropic-key"));
        assert!(request.contains("anthropic-version: 2023-06-01"));
        assert!(request.contains("Translate"));
    }

    fn task() -> AgentTask {
        AgentTask {
            id: "task".into(),
            stage: crate::domain::model::AgentStage::Translate,
            source_language: "en".into(),
            target_language: "fr".into(),
            source: "Hello".into(),
            previous_source: None,
            previous_translation: None,
            findings: Vec::new(),
            protected_tokens: Vec::new(),
        }
    }

    #[test]
    fn native_openai_compatible_provider_extracts_text_and_redacts_errors() {
        let key_env = "FANI_TEST_OPENAI_KEY";
        unsafe { std::env::set_var(key_env, "secret-openai-key") };
        let (endpoint, request) = server(
            "200 OK",
            r#"{"choices":[{"message":{"content":"Bonjour"}}]}"#,
        );
        let (output, _, _) = native_http_once(
            &config("openai-compatible", endpoint, key_env),
            &http_agent(),
            "task-2",
            "Translate",
        )
        .unwrap();
        assert_eq!(output, "Bonjour");
        assert!(
            request
                .join()
                .unwrap()
                .contains("authorization: Bearer secret-openai-key")
        );

        let (endpoint, _) = server("401 Unauthorized", r#"{"error":"secret response"}"#);
        let error = native_http_once(
            &config("openai-compatible", endpoint, key_env),
            &http_agent(),
            "task-3",
            "Translate",
        )
        .unwrap_err()
        .message;
        unsafe { std::env::remove_var(key_env) };
        assert!(error.contains("HTTP 401"));
        assert!(!error.contains("secret response"));
        assert!(!error.contains("secret-openai-key"));
    }

    #[test]
    fn authentication_failure_is_not_retried() {
        let key_env = "FANI_TEST_NO_RETRY_KEY";
        unsafe { std::env::set_var(key_env, "secret") };
        let (endpoint, request) = server("401 Unauthorized", r#"{"error":"denied"}"#);
        let mut config = config("openai-compatible", endpoint, key_env);
        config.retries = 2;
        let result = NativeHttpAgent::new(&config).execute_one(&task());
        unsafe { std::env::remove_var(key_env) };
        assert!(!result.ok);
        assert_eq!(result.attempts, 1);
        assert!(result.diagnostic.contains("HTTP 401"));
        request.join().unwrap();
    }
}
