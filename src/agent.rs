use crate::config::AgentConfig;
use crate::model::{AgentResult, AgentTask, DecisionCode};
use crate::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use anyhow::{Context, Result, anyhow};
use nix::unistd::{Pid, setpgid};
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

fn normalise(text: &str) -> String {
    let trimmed = text.trim();
    let lines: Vec<_> = trimmed.lines().collect();
    if lines.len() >= 2
        && lines[0].trim_start().starts_with("```")
        && lines.last().is_some_and(|line| line.trim() == "```")
    {
        let language = lines[0].trim().trim_matches('`').trim();
        if ["", "md", "markdown", "text"].contains(&language) {
            return lines[1..lines.len() - 1].join("\n").trim().to_owned();
        }
    }
    trimmed.to_owned()
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
    prompt: &str,
) -> Result<(String, String, f64), (String, String)> {
    let started = Instant::now();
    if prompt.len() > MAX_STDIN_BYTES {
        return Err((
            DecisionCode::AgentInvalid.as_str().into(),
            "Agent prompt exceeds input limit".into(),
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
    let input = prompt.as_bytes().to_vec();
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
    let mut diagnostic = String::from_utf8_lossy(&stderr).into_owned();
    if stderr_truncated {
        diagnostic.insert_str(0, "[diagnostic tail truncated]\n");
    }
    if !status.success() {
        return Err((
            DecisionCode::AgentExit.as_str().into(),
            format!(
                "exit {}: {}",
                status.code().unwrap_or(-1),
                diagnostic.trim()
            ),
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
    let output = normalise(&raw);
    if output.is_empty() {
        return Err((
            DecisionCode::AgentInvalid.as_str().into(),
            "Agent returned empty output".into(),
        ));
    }
    Ok((output, diagnostic, started.elapsed().as_secs_f64()))
}

pub trait AgentExecutor: Send + Sync {
    fn execute(&self, tasks: &[AgentTask]) -> Result<Vec<AgentResult>>;
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
        let prompt = crate::prompts::render(task);
        let started = Instant::now();
        let mut last_code = DecisionCode::AgentExit.as_str().to_owned();
        let mut last_message = String::new();
        for attempt in 1..=self.config.retries + 1 {
            match execute_once(&self.config, &prompt) {
                Ok((output, diagnostic, duration_s)) => {
                    return AgentResult {
                        task_id: task.id.clone(),
                        ok: true,
                        output,
                        code: None,
                        attempts: attempt,
                        duration_s,
                        diagnostic,
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
        }
    }
}

impl AgentExecutor for CommandAgent {
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

pub fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}
