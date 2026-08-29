use crate::config::AgentConfig;
use crate::model::{Task, TaskOutcome};
use crate::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use anyhow::{Context, Result, anyhow};
use nix::unistd::{Pid, setpgid};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use wait_timeout::ChildExt;

const OUTPUT_FILE_TOKEN: &str = "{output_file}";
const MAX_AGENT_ANSWER_BYTES: usize = 4 * 1024 * 1024;
const MAX_AGENT_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const STDOUT_OVERRIDE: &str = r#"

## Output channel (overrides any instruction above)

You have no file access. Do NOT attempt to write any file, and ignore any
instruction above telling you to write to a result path. Print your answer to
standard output and nothing else: no preamble, no commentary, no code-fence
wrapper around the whole answer.
"#;

#[derive(Debug)]
struct CallResult {
    ok: bool,
    text: String,
    code: Option<String>,
    duration_s: f64,
}

#[derive(Debug)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Clone, Copy)]
enum CaptureMode {
    Prefix(usize),
    Tail(usize),
}

type PipeRead = (&'static str, std::io::Result<CapturedOutput>);

struct PipeCollector {
    receiver: Receiver<PipeRead>,
    stdout: Option<std::io::Result<CapturedOutput>>,
    stderr: Option<std::io::Result<CapturedOutput>>,
}

impl PipeCollector {
    fn new(receiver: Receiver<PipeRead>) -> Self {
        Self {
            receiver,
            stdout: None,
            stderr: None,
        }
    }

    fn collect_for(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        while self.stdout.is_none() || self.stderr.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(("stdout", result)) => self.stdout = Some(result),
                Ok(("stderr", result)) => self.stderr = Some(result),
                Ok((stream, _)) => return Err(anyhow!("unknown Agent output stream {stream}")),
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("Agent output readers disconnected"));
                }
            }
        }
        Ok(true)
    }

    fn finish(self) -> Result<(CapturedOutput, CapturedOutput)> {
        let stdout = self
            .stdout
            .context("Agent stdout reader did not finish")??;
        let stderr = self
            .stderr
            .context("Agent stderr reader did not finish")??;
        Ok((stdout, stderr))
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    stream: &'static str,
    mut pipe: R,
    mode: CaptureMode,
    sender: mpsc::Sender<PipeRead>,
) {
    thread::spawn(move || {
        let result = (|| -> std::io::Result<CapturedOutput> {
            let mut bytes = Vec::new();
            let mut truncated = false;
            let mut chunk = [0_u8; 8192];
            loop {
                let count = pipe.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                match mode {
                    CaptureMode::Prefix(limit) => {
                        let remaining = limit.saturating_sub(bytes.len());
                        let keep = remaining.min(count);
                        bytes.extend_from_slice(&chunk[..keep]);
                        truncated |= keep < count;
                    }
                    CaptureMode::Tail(limit) => {
                        if count >= limit {
                            bytes.clear();
                            bytes.extend_from_slice(&chunk[count - limit..count]);
                            truncated = true;
                        } else {
                            let excess = bytes.len().saturating_add(count).saturating_sub(limit);
                            if excess > 0 {
                                bytes.drain(..excess);
                                truncated = true;
                            }
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                    }
                }
            }
            Ok(CapturedOutput { bytes, truncated })
        })();
        let _ = sender.send((stream, result));
    });
}

pub fn normalise(text: &str) -> String {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() < 2 || lines.last().map(|x| x.trim()) != Some("```") {
        return trimmed.to_string();
    }
    let opening = lines[0].trim().trim_matches('`').trim();
    if !["", "markdown", "md", "json"].contains(&opening) {
        return trimmed.to_string();
    }
    lines[1..lines.len() - 1].join("\n").trim().to_string()
}

fn read_bounded_output_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_AGENT_ANSWER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_AGENT_ANSWER_BYTES {
        return Err(anyhow!(
            "Agent output file exceeded {} bytes",
            MAX_AGENT_ANSWER_BYTES
        ));
    }
    String::from_utf8(bytes).context("Agent output file is not UTF-8")
}

pub fn build_prompt(task: &Task, findings: Option<&str>) -> String {
    let body = if task.mode == "revise" && !task.previous_translation.is_empty() {
        format!(
            r#"You are updating an existing translation, not writing a new one.

The previous source is in PREVIOUS SOURCE. Its approved translation is in
PREVIOUS TRANSLATION. The new source is in SOURCE. They differ by a match ratio
of {}: close to 1.0 means very little changed.

1. Diff PREVIOUS SOURCE against SOURCE. Only those differences may change.
2. Start from PREVIOUS TRANSLATION and edit it. Every sentence whose source did
   not change must come through byte-identical. Do not re-word it.
3. Write new target-language prose only for what genuinely changed.
4. If the source removed something, remove its translation; add new translations in place.

## PREVIOUS SOURCE

{}

## PREVIOUS TRANSLATION

{}

## ORIGINAL TRANSLATION INSTRUCTIONS

{}

## SOURCE

{}"#,
            task.match_ratio,
            task.previous_source,
            task.previous_translation,
            task.prompt,
            task.source
        )
    } else if let Some(findings) = findings {
        format!(
            r#"You are repairing a translation that failed a structural check. Do not
re-translate from scratch; make the smallest change that fixes the listed problems.

## What was wrong with the previous attempt

{findings}

Rules, unchanged from the original task:

1. Keep every @@CODE_BLOCK_n@@, @@INLINE_CODE_n@@, @@LINE_nnnn@@ token byte-identical.
2. Copy every inline code span verbatim.
3. Keep Markdown structure identical.
4. Do not introduce HTML tags that are not in the source.

## ORIGINAL TRANSLATION INSTRUCTIONS

{}

## SOURCE

{}"#,
            task.prompt, task.source
        )
    } else {
        format!("{}\n\n## SOURCE\n\n{}", task.prompt, task.source)
    };
    body + STDOUT_OVERRIDE
}

fn call_agent(agent: &AgentConfig, prompt: &str, cwd: &Path) -> CallResult {
    let started = Instant::now();
    let mut output_file: Option<NamedTempFile> = None;
    let mut cmdline = agent.cmd.clone();
    if cmdline.iter().any(|p| p.contains(OUTPUT_FILE_TOKEN)) {
        match NamedTempFile::new() {
            Ok(file) => {
                let name = file.path().display().to_string();
                cmdline = cmdline
                    .into_iter()
                    .map(|p| p.replace(OUTPUT_FILE_TOKEN, &name))
                    .collect();
                output_file = Some(file);
            }
            Err(e) => {
                return CallResult {
                    ok: false,
                    text: e.to_string(),
                    code: Some("DSP-EXIT".into()),
                    duration_s: 0.0,
                };
            }
        }
    }
    if cmdline.is_empty() {
        return CallResult {
            ok: false,
            text: "empty agent command".into(),
            code: Some("DSP-EXIT".into()),
            duration_s: 0.0,
        };
    }
    if let Err(error) = enable_subreaper() {
        return CallResult {
            ok: false,
            text: error.to_string(),
            code: Some("DSP-EXIT".into()),
            duration_s: started.elapsed().as_secs_f64(),
        };
    }
    let token = process_token();
    let mut command = match wrapped_command(&cmdline[0]) {
        Ok(command) => command,
        Err(error) => {
            return CallResult {
                ok: false,
                text: error.to_string(),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
    };
    command
        .args(&cmdline[1..])
        .current_dir(cwd)
        .env("FANI_PROCESS_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::other)
        });
    }
    let (mut child, tracker) = match spawn_tracked(&mut command, token) {
        Ok(spawned) => spawned,
        Err(e) => {
            return CallResult {
                ok: false,
                text: format!("cannot start {:?}: {e}", cmdline[0]),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
    };
    let mut stdin = child.stdin.take().unwrap();
    let prompt = prompt.as_bytes().to_vec();
    let (stdin_sender, stdin_receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = stdin.write_all(&prompt);
        let _ = stdin_sender.send(result);
    });
    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let stdout_mode = if output_file.is_some() {
        CaptureMode::Tail(MAX_AGENT_DIAGNOSTIC_BYTES)
    } else {
        CaptureMode::Prefix(MAX_AGENT_ANSWER_BYTES)
    };
    spawn_reader("stdout", stdout_pipe, stdout_mode, sender.clone());
    spawn_reader(
        "stderr",
        stderr_pipe,
        CaptureMode::Tail(MAX_AGENT_DIAGNOSTIC_BYTES),
        sender,
    );
    let mut pipes = PipeCollector::new(receiver);
    let timeout = Duration::from_secs_f64(agent.timeout_s.max(0.001));
    let deadline = started + timeout;
    let cleanup_grace = (timeout / 4).min(Duration::from_millis(100));
    let work_deadline = deadline.checked_sub(cleanup_grace).unwrap_or(started);
    let status = match child.wait_timeout(work_deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate_process_group(child, deadline, tracker);
            return CallResult {
                ok: false,
                text: format!("timed out after {:.0}s", agent.timeout_s),
                code: Some("DSP-TIMEOUT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
        Err(e) => {
            terminate_process_group(child, deadline, tracker);
            return CallResult {
                ok: false,
                text: e.to_string(),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
    };
    match stdin_receiver.recv_timeout(work_deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            terminate_process_group(child, deadline, tracker);
            return CallResult {
                ok: false,
                text: format!("cannot write agent stdin: {error}"),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
        Err(RecvTimeoutError::Timeout) => {
            terminate_process_group(child, deadline, tracker);
            return CallResult {
                ok: false,
                text: format!(
                    "timed out after {:.0}s writing Agent stdin",
                    agent.timeout_s
                ),
                code: Some("DSP-TIMEOUT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
        Err(RecvTimeoutError::Disconnected) => {
            terminate_process_group(child, deadline, tracker);
            return CallResult {
                ok: false,
                text: "Agent stdin writer disconnected".into(),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
    }
    let remaining = work_deadline.saturating_duration_since(Instant::now());
    let streams_complete = match pipes.collect_for(remaining) {
        Ok(complete) => complete,
        Err(error) => {
            terminate_process_group(child, deadline, tracker);
            return CallResult {
                ok: false,
                text: error.to_string(),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
    };
    if !streams_complete {
        terminate_process_group(child, deadline, tracker);
        return CallResult {
            ok: false,
            text: format!(
                "timed out after {:.0}s waiting for Agent output",
                agent.timeout_s
            ),
            code: Some("DSP-TIMEOUT".into()),
            duration_s: started.elapsed().as_secs_f64(),
        };
    }
    let (stdout, stderr) = match pipes.finish() {
        Ok(output) => output,
        Err(error) => {
            return CallResult {
                ok: false,
                text: error.to_string(),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
    };
    finish_process_group(tracker);
    let stdout_truncated = stdout.truncated;
    let stderr_truncated = stderr.truncated;
    let stdout = String::from_utf8_lossy(&stdout.bytes).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr.bytes).into_owned();
    if stderr_truncated {
        stderr = format!("[stderr tail truncated]\n{stderr}");
    }
    if !status.success() {
        let combined = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        let tail: String = combined
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return CallResult {
            ok: false,
            text: format!("exit {}: {}", status.code().unwrap_or(-1), tail.trim()),
            code: Some("DSP-EXIT".into()),
            duration_s: started.elapsed().as_secs_f64(),
        };
    }
    let raw = if let Some(file) = output_file {
        match read_bounded_output_file(file.path()) {
            Ok(output) => output,
            Err(error) => {
                return CallResult {
                    ok: false,
                    text: error.to_string(),
                    code: Some("DSP-EXIT".into()),
                    duration_s: started.elapsed().as_secs_f64(),
                };
            }
        }
    } else {
        if stdout_truncated {
            return CallResult {
                ok: false,
                text: format!("Agent stdout exceeded {} bytes", MAX_AGENT_ANSWER_BYTES),
                code: Some("DSP-EXIT".into()),
                duration_s: started.elapsed().as_secs_f64(),
            };
        }
        stdout
    };
    let text = normalise(&raw);
    if text.trim().is_empty() {
        return CallResult {
            ok: false,
            text: "agent produced empty output".into(),
            code: Some("DSP-EMPTY".into()),
            duration_s: started.elapsed().as_secs_f64(),
        };
    }
    CallResult {
        ok: true,
        text,
        code: None,
        duration_s: started.elapsed().as_secs_f64(),
    }
}

pub trait DispatchApi: Send + Sync {
    fn run(
        &self,
        files: &[PathBuf],
        kind: &str,
        findings: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<TaskOutcome>>;
}

#[derive(Clone)]
pub struct Dispatcher {
    root: PathBuf,
    agent: AgentConfig,
}

impl Dispatcher {
    pub fn new(root: &Path, agent: &AgentConfig) -> Self {
        Self {
            root: root.to_path_buf(),
            agent: agent.clone(),
        }
    }

    pub fn run(
        &self,
        files: &[PathBuf],
        kind: &str,
        findings: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<TaskOutcome>> {
        self.run_recording(files, kind, findings, |_| Ok(()))
    }

    pub fn run_recording<F>(
        &self,
        files: &[PathBuf],
        kind: &str,
        findings: &std::collections::HashMap<String, String>,
        record: F,
    ) -> Result<Vec<TaskOutcome>>
    where
        F: Fn(&TaskOutcome) -> Result<()> + Sync,
    {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let files = Arc::new(files.to_vec());
        let next = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::<(usize, TaskOutcome)>::new()));
        let errors = Arc::new(Mutex::new(Vec::<anyhow::Error>::new()));
        let workers = self.agent.concurrency.max(1).min(files.len());
        thread::scope(|scope| {
            for _ in 0..workers {
                let files = Arc::clone(&files);
                let next = Arc::clone(&next);
                let results = Arc::clone(&results);
                let errors = Arc::clone(&errors);
                let findings = findings.clone();
                let dispatcher = self.clone();
                let kind = kind.to_string();
                let record = &record;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= files.len() {
                            break;
                        }
                        match dispatcher.run_one(
                            &files[index],
                            &kind,
                            findings.get(
                                files[index]
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or(""),
                            ),
                        ) {
                            Ok((outcome, fatal)) => match record(&outcome) {
                                Ok(()) => {
                                    if let Some(error) = fatal {
                                        errors.lock().unwrap().push(error);
                                    } else {
                                        results.lock().unwrap().push((index, outcome));
                                    }
                                }
                                Err(error) => errors.lock().unwrap().push(error),
                            },
                            Err(err) => errors.lock().unwrap().push(err),
                        }
                    }
                });
            }
        });
        if let Some(err) = errors.lock().unwrap().pop() {
            return Err(err);
        }
        let mut out = results.lock().unwrap().clone();
        out.sort_by_key(|(index, _)| *index);
        Ok(out.into_iter().map(|(_, item)| item).collect())
    }

    fn run_one(
        &self,
        task_file: &Path,
        kind: &str,
        findings: Option<&String>,
    ) -> Result<(TaskOutcome, Option<anyhow::Error>)> {
        let task: Task = serde_json::from_str(&fs::read_to_string(task_file)?)
            .with_context(|| format!("invalid task JSON: {}", task_file.display()))?;
        if kind != "review" && task.chunk_id.is_empty() {
            return Err(anyhow!(
                "translation task is missing chunk_id: {}",
                task_file.display()
            ));
        }
        let task_id = if task.task_id.is_empty() {
            task_file
                .file_stem()
                .and_then(|x| x.to_str())
                .unwrap_or("unknown")
                .to_string()
        } else {
            task.task_id.clone()
        };
        let prompt = if kind == "review" {
            format!("{}{}", task.prompt, STDOUT_OVERRIDE)
        } else {
            build_prompt(&task, findings.map(String::as_str))
        };
        let started = Instant::now();
        let mut last = "unknown failure".to_string();
        let mut code = Some("DSP-EMPTY".to_string());
        for attempt in 0..=self.agent.retries {
            let result = call_agent(&self.agent, &prompt, &self.root);
            if !result.ok {
                last = result.text;
                code = result.code;
                continue;
            }
            if kind == "review" {
                match write_review(&self.root, &task, &result.text) {
                    Ok(true) => {
                        return Ok((
                            TaskOutcome {
                                task_id,
                                ok: true,
                                code: None,
                                attempts: attempt + 1,
                                duration_s: started.elapsed().as_secs_f64(),
                                message: String::new(),
                            },
                            None,
                        ));
                    }
                    Ok(false) => {
                        last = "agent output was not valid review JSON".into();
                        code = Some("DSP-EMPTY".into());
                        continue;
                    }
                    Err(error) => {
                        let message = error.to_string();
                        return Ok((
                            TaskOutcome {
                                task_id,
                                ok: false,
                                code: Some("DSP-EXIT".into()),
                                attempts: attempt + 1,
                                duration_s: started.elapsed().as_secs_f64(),
                                message: message.clone(),
                            },
                            Some(anyhow!(message)),
                        ));
                    }
                }
            }
            let text = unwrap_translation(&result.text, &task.chunk_id);
            if text.trim().is_empty() {
                last = "agent output was empty after unwrapping".into();
                code = Some("DSP-EMPTY".into());
                continue;
            }
            return match write_translation(&self.root, &task, &text) {
                Ok(()) => Ok((
                    TaskOutcome {
                        task_id,
                        ok: true,
                        code: None,
                        attempts: attempt + 1,
                        duration_s: result.duration_s,
                        message: String::new(),
                    },
                    None,
                )),
                Err(error) => {
                    let message = error.to_string();
                    Ok((
                        TaskOutcome {
                            task_id,
                            ok: false,
                            code: Some("DSP-EXIT".into()),
                            attempts: attempt + 1,
                            duration_s: result.duration_s,
                            message: message.clone(),
                        },
                        Some(anyhow!(message)),
                    ))
                }
            };
        }
        Ok((
            TaskOutcome {
                task_id,
                ok: false,
                code,
                attempts: self.agent.retries + 1,
                duration_s: started.elapsed().as_secs_f64(),
                message: last,
            },
            None,
        ))
    }
}

impl DispatchApi for Dispatcher {
    fn run(
        &self,
        files: &[PathBuf],
        kind: &str,
        findings: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<TaskOutcome>> {
        Dispatcher::run(self, files, kind, findings)
    }
}

fn safe_result_path(root: &Path, rel: &Path) -> Result<PathBuf> {
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(anyhow!(
            "task result_path escapes repository: {}",
            rel.display()
        ));
    }
    let root = fs::canonicalize(root)?;
    let mut candidate = root.clone();
    for component in rel.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        candidate.push(part);
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!(
                    "task result_path crosses a repository symlink: {}",
                    rel.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(candidate)
}

fn unwrap_translation(text: &str, chunk_id: &str) -> String {
    if text.trim_start().starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            if let Some(translated) = value.get("translated_text").and_then(Value::as_str) {
                if value
                    .get("chunk_id")
                    .and_then(Value::as_str)
                    .is_none_or(|id| id == chunk_id)
                {
                    return translated.to_string();
                }
            }
        }
    }
    text.to_string()
}

fn write_translation(root: &Path, task: &Task, text: &str) -> Result<()> {
    let path = safe_result_path(root, &task.result_path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(&json!({"chunk_id": task.chunk_id, "translated_text": text}))?
            + "\n",
    )?;
    Ok(())
}

fn write_review(root: &Path, task: &Task, text: &str) -> Result<bool> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Ok(false);
    };
    if !value.get("findings").is_none_or(Value::is_array) {
        return Ok(false);
    }
    let path = safe_result_path(root, &task.result_path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(&value)? + "\n")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn fake_agent(mode: &str) -> AgentConfig {
        AgentConfig {
            name: format!("fake-{mode}"),
            cmd: vec![
                "python3".into(),
                format!(
                    "{}/tests/fixtures/fake_agent.py",
                    env!("CARGO_MANIFEST_DIR")
                ),
                mode.into(),
            ],
            stages: vec!["translate".into(), "revision".into()],
            concurrency: 4,
            timeout_s: 2.0,
            retries: 0,
            enabled: true,
        }
    }

    fn task(root: &Path, id: &str) -> PathBuf {
        let path = root.join(format!("tasks/{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string(&json!({
            "task_id": id, "chunk_id": "body:1", "prompt": "Translate.",
            "source": "# Title\n\nText\n\n@@CODE_BLOCK_0@@", "result_path": format!("results/{id}.json")
        })).unwrap()).unwrap();
        path
    }

    #[test]
    fn dispatches_and_writes_skill_result() {
        let tmp = tempdir().unwrap();
        let out = Dispatcher::new(tmp.path(), &fake_agent("ok"))
            .run(&[task(tmp.path(), "t1")], "tasks", &HashMap::new())
            .unwrap();
        assert!(out[0].ok);
        let result: Value =
            serde_json::from_str(&fs::read_to_string(tmp.path().join("results/t1.json")).unwrap())
                .unwrap();
        assert!(result["translated_text"].as_str().unwrap().contains("[zh]"));
    }

    #[test]
    fn failure_and_timeout_do_not_write_result() {
        let tmp = tempdir().unwrap();
        let t = task(tmp.path(), "fail");
        let out = Dispatcher::new(tmp.path(), &fake_agent("fail"))
            .run(&[t], "tasks", &HashMap::new())
            .unwrap();
        assert_eq!(out[0].code.as_deref(), Some("DSP-EXIT"));
        assert!(!tmp.path().join("results/fail.json").exists());

        let t = task(tmp.path(), "slow");
        let mut slow = fake_agent("slow");
        slow.timeout_s = 0.1;
        let out = Dispatcher::new(tmp.path(), &slow)
            .run(&[t], "tasks", &HashMap::new())
            .unwrap();
        assert_eq!(out[0].code.as_deref(), Some("DSP-TIMEOUT"));
        assert!(!tmp.path().join("results/slow.json").exists());
    }

    #[test]
    fn prompt_write_is_bounded_by_agent_timeout() {
        let tmp = tempdir().unwrap();
        let task_path = task(tmp.path(), "noread");
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(&task_path).unwrap()).unwrap();
        value["source"] = Value::String("x".repeat(2 * 1024 * 1024));
        fs::write(&task_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let mut agent = fake_agent("noread");
        agent.timeout_s = 0.2;
        let started = Instant::now();
        let out = Dispatcher::new(tmp.path(), &agent)
            .run(&[task_path], "tasks", &HashMap::new())
            .unwrap();

        assert_eq!(out[0].code.as_deref(), Some("DSP-TIMEOUT"));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!tmp.path().join("results/noread.json").exists());
    }

    #[test]
    fn large_agent_outputs_are_bounded() {
        let tmp = tempdir().unwrap();
        let mut flood = fake_agent("flood");
        flood.timeout_s = 5.0;
        let out = Dispatcher::new(tmp.path(), &flood)
            .run(&[task(tmp.path(), "pipe-flood")], "tasks", &HashMap::new())
            .unwrap();
        assert_eq!(out[0].code.as_deref(), Some("DSP-EXIT"));
        assert!(out[0].message.contains("stdout exceeded"));
        assert!(!tmp.path().join("results/pipe-flood.json").exists());

        let mut output_flood = fake_agent("outputflood");
        output_flood.timeout_s = 5.0;
        output_flood.cmd.push(OUTPUT_FILE_TOKEN.into());
        let out = Dispatcher::new(tmp.path(), &output_flood)
            .run(&[task(tmp.path(), "file-flood")], "tasks", &HashMap::new())
            .unwrap();
        assert_eq!(out[0].code.as_deref(), Some("DSP-EXIT"));
        assert!(out[0].message.contains("output file exceeded"));
        assert!(!tmp.path().join("results/file-flood.json").exists());
    }

    #[test]
    fn timeout_terminates_descendant_process_group() {
        let tmp = tempdir().unwrap();
        let pid_file = tmp.path().join("child.pid");
        let mut agent = fake_agent("forkslow");
        agent.timeout_s = 0.2;
        agent.cmd.insert(0, "env".into());
        agent
            .cmd
            .insert(1, format!("FAKE_AGENT_CHILD_PID={}", pid_file.display()));
        let out = Dispatcher::new(tmp.path(), &agent)
            .run(&[task(tmp.path(), "fork")], "tasks", &HashMap::new())
            .unwrap();
        assert_eq!(out[0].code.as_deref(), Some("DSP-TIMEOUT"));
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "descendant process {pid} was not reaped"
        );
    }

    #[test]
    fn successful_agent_reaps_detached_background_child() {
        let tmp = tempdir().unwrap();
        let pid_file = tmp.path().join("orphan-child.pid");
        let mut agent = fake_agent("orphanok");
        agent.cmd.insert(0, "env".into());
        agent
            .cmd
            .insert(1, format!("FAKE_AGENT_CHILD_PID={}", pid_file.display()));
        let out = Dispatcher::new(tmp.path(), &agent)
            .run(&[task(tmp.path(), "orphan")], "tasks", &HashMap::new())
            .unwrap();
        assert!(out[0].ok);
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let reaped = (0..100).any(|_| {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                true
            } else {
                thread::sleep(Duration::from_millis(10));
                false
            }
        });
        assert!(reaped, "successful Agent left detached child {pid}");
    }

    #[test]
    fn timeout_kills_and_reaps_descendant_that_escaped_process_group() {
        let tmp = tempdir().unwrap();
        let pid_file = tmp.path().join("escaped-child.pid");
        let mut agent = fake_agent("forkescape");
        agent.timeout_s = 0.3;
        agent.cmd.insert(0, "env".into());
        agent
            .cmd
            .insert(1, format!("FAKE_AGENT_CHILD_PID={}", pid_file.display()));
        let started = Instant::now();
        let out = Dispatcher::new(tmp.path(), &agent)
            .run(&[task(tmp.path(), "escaped")], "tasks", &HashMap::new())
            .unwrap();

        assert_eq!(out[0].code.as_deref(), Some("DSP-TIMEOUT"));
        assert!(started.elapsed() < Duration::from_millis(600));
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "escaped descendant {pid} was not reaped"
        );
    }

    #[test]
    fn leader_exit_with_descendant_retaining_pipes_returns_timeout() {
        let tmp = tempdir().unwrap();
        let pid_file = tmp.path().join("holding-child.pid");
        let mut agent = fake_agent("forkhold");
        agent.timeout_s = 0.2;
        agent.cmd.insert(0, "env".into());
        agent
            .cmd
            .insert(1, format!("FAKE_AGENT_CHILD_PID={}", pid_file.display()));
        let started = Instant::now();
        let out = Dispatcher::new(tmp.path(), &agent)
            .run(&[task(tmp.path(), "leader-exit")], "tasks", &HashMap::new())
            .unwrap();

        assert_eq!(out[0].code.as_deref(), Some("DSP-TIMEOUT"));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!tmp.path().join("results/leader-exit.json").exists());
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "descendant process {pid} was not reaped"
        );
    }

    #[test]
    fn enforces_configured_concurrency() {
        let tmp = tempdir().unwrap();
        let counter = tmp.path().join("concurrency.txt");
        fs::write(&counter, "0 0").unwrap();
        let mut agent = fake_agent("concurrency");
        agent.concurrency = 3;
        agent.cmd.insert(0, "env".into());
        agent.cmd.insert(
            1,
            format!("FAKE_AGENT_CONCURRENCY_FILE={}", counter.display()),
        );
        let tasks: Vec<_> = (0..5).map(|i| task(tmp.path(), &format!("t{i}"))).collect();
        let out = Dispatcher::new(tmp.path(), &agent)
            .run(&tasks, "tasks", &HashMap::new())
            .unwrap();
        assert!(out.iter().all(|x| x.ok));
        let parts: Vec<usize> = fs::read_to_string(counter)
            .unwrap()
            .split_whitespace()
            .map(|x| x.parse().unwrap())
            .collect();
        assert_eq!(parts[0], 0);
        assert_eq!(parts[1], 3);
    }

    #[test]
    fn translation_tasks_still_require_chunk_id() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("task.json");
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "task_id":"x", "result_path":"results/x.json", "source":"s"
            }))
            .unwrap(),
        )
        .unwrap();
        let error = Dispatcher::new(tmp.path(), &fake_agent("ok"))
            .run(&[path], "tasks", &HashMap::new())
            .unwrap_err();
        assert!(error.to_string().contains("missing chunk_id"));
        assert!(!tmp.path().join("results/x.json").exists());
    }

    #[test]
    fn records_post_processing_failures_as_agent_outcomes() {
        let tmp = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("escape")).unwrap();
        let path = tmp.path().join("task.json");
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "task_id":"x", "chunk_id":"c", "result_path":"escape/pwn.json", "source":"s"
            }))
            .unwrap(),
        )
        .unwrap();
        let recorded = Mutex::new(Vec::new());
        let error = Dispatcher::new(tmp.path(), &fake_agent("ok"))
            .run_recording(&[path], "tasks", &HashMap::new(), |outcome| {
                recorded.lock().unwrap().push(outcome.clone());
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("symlink"));
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].code.as_deref(), Some("DSP-EXIT"));
    }

    #[test]
    fn rejects_result_path_through_repository_symlink() {
        let tmp = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("escape")).unwrap();
        let path = tmp.path().join("task.json");
        fs::write(
            &path,
            serde_json::to_string(&json!({
                "task_id":"x", "chunk_id":"c", "result_path":"escape/pwn.json", "source":"s"
            }))
            .unwrap(),
        )
        .unwrap();
        let error = Dispatcher::new(tmp.path(), &fake_agent("ok"))
            .run(&[path], "tasks", &HashMap::new())
            .unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert!(!outside.path().join("pwn.json").exists());
    }

    #[test]
    fn rejects_escaping_result_path() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("task.json");
        fs::write(
            &path,
            serde_json::to_string(
                &json!({"task_id":"x","chunk_id":"c","result_path":"../x","source":"s"}),
            )
            .unwrap(),
        )
        .unwrap();
        let error = Dispatcher::new(tmp.path(), &fake_agent("ok"))
            .run(&[path], "tasks", &HashMap::new())
            .unwrap_err();
        assert!(error.to_string().contains("escapes"));
    }
}
