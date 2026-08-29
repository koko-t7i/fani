use crate::model::{ApplyResult, PlanResult, ReviewCollectResult, ReviewPlanResult, VerifyResult};
use crate::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use anyhow::{Context, Result, anyhow};
use nix::unistd::{Pid, setpgid};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

#[derive(Clone, Debug)]
pub struct Skill {
    run_sh: PathBuf,
    root: PathBuf,
    state_dir: String,
    timeout: Duration,
}

#[derive(Debug)]
pub struct SkillResult<T> {
    pub returncode: i32,
    pub data: T,
}

pub trait SkillApi {
    fn state_path(&self) -> PathBuf;
    fn work_dir(&self, run_id: &str) -> PathBuf;
    fn plan(
        &self,
        lang: &str,
        paths: &[String],
        exclude: &[String],
        max_tasks: usize,
        repair: Option<&Path>,
    ) -> Result<SkillResult<PlanResult>>;
    fn apply(&self, run_id: &str) -> Result<SkillResult<ApplyResult>>;
    fn verify(&self, lang: &str) -> Result<SkillResult<VerifyResult>>;
    fn review_plan(
        &self,
        lang: &str,
        mode: &str,
        run_id: Option<&str>,
    ) -> Result<SkillResult<ReviewPlanResult>>;
    fn review_collect(&self, run_id: &str) -> Result<SkillResult<ReviewCollectResult>>;
}

const MAX_SKILL_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_SKILL_DIAGNOSTIC_BYTES: usize = 64 * 1024;

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
                Ok((stream, _)) => return Err(anyhow!("unknown skill output stream {stream}")),
                Err(RecvTimeoutError::Timeout) => return Ok(false),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("i18n skill output readers disconnected"));
                }
            }
        }
        Ok(true)
    }

    fn finish(self) -> Result<(CapturedOutput, CapturedOutput)> {
        let stdout = self
            .stdout
            .context("i18n skill stdout reader did not finish")??;
        let stderr = self
            .stderr
            .context("i18n skill stderr reader did not finish")??;
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

impl Skill {
    pub fn new(skill_dir: &Path, root: &Path, state_dir: &str) -> Self {
        Self {
            run_sh: skill_dir.join("scripts/run.sh"),
            root: root.to_path_buf(),
            state_dir: state_dir.to_string(),
            timeout: Duration::from_secs(900),
        }
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join(&self.state_dir).join("state.json")
    }
    pub fn work_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(&self.state_dir).join("work").join(run_id)
    }

    fn run<T: DeserializeOwned + Default>(&self, args: Vec<OsString>) -> Result<SkillResult<T>> {
        enable_subreaper()?;
        let started = Instant::now();
        let token = process_token();
        let mut cmd = wrapped_command(&self.run_sh)?;
        cmd.args(&args)
            .current_dir(&self.root)
            .env("FANI_PROCESS_TOKEN", &token)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(|| {
                setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::other)
            });
        }
        let (mut child, tracker) = spawn_tracked(&mut cmd, token)
            .with_context(|| format!("cannot execute {}", self.run_sh.display()))?;
        let stdout_pipe = child.stdout.take().context("cannot capture skill stdout")?;
        let stderr_pipe = child.stderr.take().context("cannot capture skill stderr")?;
        let (sender, receiver) = mpsc::channel();
        spawn_reader(
            "stdout",
            stdout_pipe,
            CaptureMode::Prefix(MAX_SKILL_JSON_BYTES),
            sender.clone(),
        );
        spawn_reader(
            "stderr",
            stderr_pipe,
            CaptureMode::Tail(MAX_SKILL_DIAGNOSTIC_BYTES),
            sender,
        );
        let mut pipes = PipeCollector::new(receiver);
        let status = match child.wait_timeout(self.timeout.saturating_sub(started.elapsed())) {
            Ok(Some(status)) => status,
            Ok(None) => {
                terminate_process_group(child, Instant::now() + Duration::from_secs(2), tracker);
                let _ = pipes.collect_for(Duration::from_secs(2));
                return Err(anyhow!(
                    "i18n skill timed out after {}s",
                    self.timeout.as_secs()
                ));
            }
            Err(err) => {
                terminate_process_group(child, Instant::now() + Duration::from_secs(2), tracker);
                let _ = pipes.collect_for(Duration::from_secs(2));
                return Err(err).context("cannot wait for i18n skill");
            }
        };
        let remaining = self.timeout.saturating_sub(started.elapsed());
        if !pipes.collect_for(remaining)? {
            terminate_process_group(child, Instant::now() + Duration::from_secs(2), tracker);
            if !pipes.collect_for(Duration::from_secs(2))? {
                return Err(anyhow!(
                    "i18n skill descendants did not close output streams after timeout"
                ));
            }
            return Err(anyhow!(
                "i18n skill timed out after {}s waiting for output streams",
                self.timeout.as_secs()
            ));
        }
        let (stdout, stderr) = pipes.finish()?;
        finish_process_group(tracker);
        let stdout_truncated = stdout.truncated;
        let stderr_truncated = stderr.truncated;
        let stdout = String::from_utf8_lossy(&stdout.bytes).into_owned();
        let mut stderr = String::from_utf8_lossy(&stderr.bytes).into_owned();
        if stderr_truncated {
            stderr = format!("[stderr tail truncated]\n{stderr}");
        }
        let code = status.code().unwrap_or(2);
        if code == 2 {
            return Err(anyhow!(
                "{} failed (exit 2): {}",
                args.first()
                    .map(|x| x.to_string_lossy())
                    .unwrap_or_default(),
                stderr.trim()
            ));
        }
        if ![0, 1, 3].contains(&code) {
            return Err(anyhow!(
                "{} failed (exit {code}): {}",
                args.first()
                    .map(|x| x.to_string_lossy())
                    .unwrap_or_default(),
                stderr.trim()
            ));
        }
        if stdout_truncated {
            return Err(anyhow!(
                "{} stdout exceeded {} bytes",
                args.first()
                    .map(|x| x.to_string_lossy())
                    .unwrap_or_default(),
                MAX_SKILL_JSON_BYTES
            ));
        }
        let data = if stdout.trim().is_empty() {
            T::default()
        } else {
            serde_json::from_str(&stdout).with_context(|| {
                format!(
                    "{} produced output that is not JSON: {}",
                    args.first()
                        .map(|x| x.to_string_lossy())
                        .unwrap_or_default(),
                    stdout.chars().take(400).collect::<String>()
                )
            })?
        };
        Ok(SkillResult {
            returncode: code,
            data,
        })
    }

    fn common(&self) -> Vec<OsString> {
        vec![
            "--root".into(),
            self.root.as_os_str().into(),
            "--state-dir".into(),
            self.state_dir.clone().into(),
        ]
    }

    pub fn plan(
        &self,
        lang: &str,
        paths: &[String],
        exclude: &[String],
        max_tasks: usize,
        repair: Option<&Path>,
    ) -> Result<SkillResult<PlanResult>> {
        let mut args: Vec<OsString> = vec!["plan".into()];
        args.extend(self.common());
        args.extend([
            "--lang".into(),
            lang.into(),
            "--max-tasks".into(),
            max_tasks.to_string().into(),
            "--json".into(),
        ]);
        if !paths.is_empty() {
            args.push("--paths".into());
            args.extend(paths.iter().map(Into::into));
        }
        if !exclude.is_empty() {
            args.push("--exclude".into());
            args.extend(exclude.iter().map(Into::into));
        }
        if let Some(repair) = repair {
            args.extend(["--repair".into(), repair.as_os_str().into()]);
        }
        self.run(args)
    }

    pub fn apply(&self, run_id: &str) -> Result<SkillResult<ApplyResult>> {
        let mut args: Vec<OsString> = vec!["apply".into()];
        args.extend(self.common());
        args.extend(["--run".into(), run_id.into(), "--json".into()]);
        self.run(args)
    }

    pub fn verify(&self, lang: &str) -> Result<SkillResult<VerifyResult>> {
        let mut args: Vec<OsString> = vec!["verify".into()];
        args.extend(self.common());
        args.extend(["--lang".into(), lang.into(), "--json".into()]);
        self.run(args)
    }

    pub fn review_plan(
        &self,
        lang: &str,
        mode: &str,
        run_id: Option<&str>,
    ) -> Result<SkillResult<ReviewPlanResult>> {
        let mut args: Vec<OsString> = vec!["review".into(), "plan".into()];
        args.extend(self.common());
        args.extend([
            "--lang".into(),
            lang.into(),
            "--mode".into(),
            mode.into(),
            "--json".into(),
        ]);
        if let Some(id) = run_id {
            args.extend(["--run".into(), id.into()]);
        }
        self.run(args)
    }

    pub fn review_collect(&self, run_id: &str) -> Result<SkillResult<ReviewCollectResult>> {
        let mut args: Vec<OsString> = vec!["review".into(), "collect".into()];
        args.extend(self.common());
        args.extend(["--run".into(), run_id.into(), "--json".into()]);
        self.run(args)
    }
}

impl SkillApi for Skill {
    fn state_path(&self) -> PathBuf {
        Skill::state_path(self)
    }
    fn work_dir(&self, run_id: &str) -> PathBuf {
        Skill::work_dir(self, run_id)
    }
    fn plan(
        &self,
        lang: &str,
        paths: &[String],
        exclude: &[String],
        max_tasks: usize,
        repair: Option<&Path>,
    ) -> Result<SkillResult<PlanResult>> {
        Skill::plan(self, lang, paths, exclude, max_tasks, repair)
    }
    fn apply(&self, run_id: &str) -> Result<SkillResult<ApplyResult>> {
        Skill::apply(self, run_id)
    }
    fn verify(&self, lang: &str) -> Result<SkillResult<VerifyResult>> {
        Skill::verify(self, lang)
    }
    fn review_plan(
        &self,
        lang: &str,
        mode: &str,
        run_id: Option<&str>,
    ) -> Result<SkillResult<ReviewPlanResult>> {
        Skill::review_plan(self, lang, mode, run_id)
    }
    fn review_collect(&self, run_id: &str) -> Result<SkillResult<ReviewCollectResult>> {
        Skill::review_collect(self, run_id)
    }
}

pub fn task_files(work: &Path, kind: &str) -> Vec<PathBuf> {
    let dir = work.join(if kind == "review" { "review" } else { "tasks" });
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|x| x == "json") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

pub fn slug(rel: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in rel.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else {
            dash = true;
        }
    }
    out
}

pub fn findings_for_tasks(verify: &VerifyResult, tasks: &[PathBuf]) -> HashMap<String, String> {
    let mut by_file: HashMap<String, Vec<String>> = HashMap::new();
    for finding in &verify.findings {
        if finding.get("severity").and_then(Value::as_str) != Some("error") {
            continue;
        }
        let Some(file) = finding.get("file").and_then(Value::as_str) else {
            continue;
        };
        let mut line = format!(
            "{}: {}",
            finding.get("code").and_then(Value::as_str).unwrap_or(""),
            finding.get("message").and_then(Value::as_str).unwrap_or("")
        );
        if finding.get("expected").is_some() {
            line.push_str(&format!(
                "\n  expected={}\n  actual  ={}",
                finding["expected"], finding["actual"]
            ));
        }
        by_file.entry(file.to_string()).or_default().push(line);
    }
    let mut out = HashMap::new();
    for (file, lines) in by_file {
        let prefix = slug(&file);
        for task in tasks {
            if let Some(stem) = task.file_stem().and_then(|x| x.to_str()) {
                if stem.starts_with(&prefix) {
                    out.insert(stem.to_string(), lines.join("\n"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{TempDir, tempdir};

    fn scripted_skill(script: &str) -> (TempDir, Skill) {
        let tmp = tempdir().unwrap();
        let skill_dir = tmp.path().join("skill");
        let root = tmp.path().join("repo");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::create_dir_all(&root).unwrap();
        let run_sh = skill_dir.join("scripts/run.sh");
        fs::write(&run_sh, script).unwrap();
        let mut permissions = fs::metadata(&run_sh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&run_sh, permissions).unwrap();
        let skill = Skill::new(&skill_dir, &root, ".state");
        (tmp, skill)
    }

    #[test]
    fn drains_large_stdout_and_stderr_while_skill_is_running() {
        let (_tmp, mut skill) = scripted_skill(
            r#"#!/bin/sh
exec python3 - <<'PY'
import json
import sys
sys.stderr.write("e" * (1024 * 1024))
sys.stderr.flush()
print(json.dumps({
    "run_id": "large-output",
    "task_count": 1,
    "conflicts": [],
    "files": [],
    "fuzzy_matched": 0,
    "truncated_tasks": 0,
    "padding": "x" * (1024 * 1024),
}))
PY
"#,
        );
        skill.timeout = Duration::from_secs(5);
        let result = skill.plan("zh-CN", &[], &[], 1, None).unwrap();

        assert_eq!(result.returncode, 0);
        assert_eq!(result.data.run_id, "large-output");
        assert_eq!(result.data.task_count, 1);
    }

    #[test]
    fn rejects_oversized_skill_stdout_without_unbounded_capture() {
        let (_tmp, mut skill) = scripted_skill(
            r#"#!/bin/sh
exec python3 - <<'PY'
import sys
sys.stderr.write("e" * (1024 * 1024))
sys.stderr.flush()
sys.stdout.write("x" * (5 * 1024 * 1024))
PY
"#,
        );
        skill.timeout = Duration::from_secs(5);
        let error = skill.plan("zh-CN", &[], &[], 1, None).unwrap_err();
        assert!(error.to_string().contains("stdout exceeded"));
    }

    #[test]
    fn kills_descendant_that_retains_output_pipes_after_leader_exits() {
        let tmp = tempdir().unwrap();
        let pid_file = tmp.path().join("descendant.pid");
        let script = format!(
            r#"#!/bin/sh
exec python3 - <<'PY'
import json
import os
import signal
import time
pid = os.fork()
if pid == 0:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    with open(r"{}", "w", encoding="utf-8") as handle:
        handle.write(str(os.getpid()))
    time.sleep(30)
    os._exit(0)
print(json.dumps({{"run_id": "leader-exited", "task_count": 1}}), flush=True)
os._exit(0)
PY
"#,
            pid_file.display()
        );
        let (_skill_tmp, mut skill) = scripted_skill(&script);
        skill.timeout = Duration::from_millis(300);
        let started = Instant::now();
        let error = skill.plan("zh-CN", &[], &[], 1, None).unwrap_err();

        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        let gone = (0..50).any(|_| {
            if nix::sys::signal::kill(Pid::from_raw(pid), None).is_err() {
                true
            } else {
                thread::sleep(Duration::from_millis(20));
                false
            }
        });
        assert!(
            gone,
            "skill descendant {pid} survived process-group timeout"
        );
    }

    #[test]
    fn invalid_multibyte_json_is_reported_without_panicking() {
        let (_tmp, mut skill) = scripted_skill(
            r#"#!/bin/sh
exec python3 - <<'PY'
print("译" * 500)
PY
"#,
        );
        skill.timeout = Duration::from_secs(2);
        let error = skill.plan("zh-CN", &[], &[], 1, None).unwrap_err();
        assert!(error.to_string().contains("not JSON"), "{error:#}");
    }
}
