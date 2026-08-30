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
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

type ReadResult = (&'static str, std::io::Result<(Vec<u8>, bool)>);

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
        let provider_fingerprint = format!("{:x}", identity.finalize());
        let results = CommandAgent::new(config).execute(tasks)?;
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
