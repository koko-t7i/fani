use crate::adapters::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use crate::application::ports::{
    DocumentationCheck, DocumentationCheckFailure, DocumentationChecker, PublicationFile,
};
use crate::application::settings::RepoConfig;
use anyhow::{Context, Result, anyhow, bail};
use nix::unistd::{Pid, setpgid};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};
use std::process::Stdio;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wait_timeout::ChildExt;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

type ReadResult = (&'static str, std::io::Result<(Vec<u8>, bool)>);

fn spawn_reader<R: Read + Send + 'static>(
    name: &'static str,
    mut reader: R,
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
                let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
                let keep = remaining.min(count);
                output.extend_from_slice(&chunk[..keep]);
                truncated |= keep != count;
            }
            Ok((output, truncated))
        })();
        let _ = sender.send((name, result));
    });
}

fn isolated_environment(command: &mut std::process::Command, home: &Path) {
    let path = std::env::var_os("PATH");
    command
        .env_clear()
        .env("HOME", home)
        .env("LANG", "C.UTF-8")
        .env("FANI_DOCUMENTATION_CHECK", "1");
    if let Some(path) = path {
        command.env("PATH", path);
    }
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<()> {
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .context("cannot run git for documentation staging")?;
    if !output.status.success() {
        bail!(
            "git documentation staging failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn candidate_path(root: &Path, relative: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("documentation candidate path escapes staging: {relative}");
    }
    Ok(root.join(path))
}

fn prepare_staging(
    repo: &RepoConfig,
    source_revision: &str,
    files: &[PublicationFile],
) -> Result<TempDir> {
    let sandbox = tempfile::tempdir().context("cannot create documentation check staging")?;
    let staging = sandbox.path().join("candidate");
    let repo_arg = repo.path.to_string_lossy().into_owned();
    let staging_arg = staging.to_string_lossy().into_owned();
    run_git(
        &[
            "clone",
            "--quiet",
            "--local",
            "--no-hardlinks",
            "--no-checkout",
            &repo_arg,
            &staging_arg,
        ],
        None,
    )?;
    run_git(
        &["checkout", "--quiet", "--detach", source_revision],
        Some(&staging),
    )?;
    std::fs::remove_dir_all(staging.join(".git"))
        .context("cannot remove Git metadata from documentation staging")?;
    for file in files {
        let path = candidate_path(&staging, &file.path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "cannot create documentation staging path {}",
                    parent.display()
                )
            })?;
        }
        std::fs::write(&path, &file.content)
            .with_context(|| format!("cannot write documentation candidate {}", file.path))?;
        let actual = std::fs::read(&path)
            .with_context(|| format!("cannot verify documentation candidate {}", file.path))?;
        if actual != file.content {
            bail!("documentation staging bytes changed for {}", file.path);
        }
    }
    Ok(sandbox)
}

fn execute_command(
    repo: &RepoConfig,
    source_revision: &str,
    files: &[PublicationFile],
    argv: &[String],
) -> Result<Option<DocumentationCheckFailure>> {
    enable_subreaper()?;
    let sandbox = prepare_staging(repo, source_revision, files)?;
    let staging = sandbox.path().join("candidate");
    let home = sandbox.path().join("home");
    std::fs::create_dir(&home).context("cannot create documentation check HOME")?;
    let program = argv
        .first()
        .ok_or_else(|| anyhow!("empty documentation check command"))?;
    let token = process_token();
    let mut command = wrapped_command(program)?;
    command
        .args(&argv[1..])
        .current_dir(&staging)
        .env("FANI_PROCESS_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolated_environment(&mut command, &home);
    command.env("FANI_PROCESS_TOKEN", &token);
    unsafe {
        command.pre_exec(|| {
            setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::other)
        });
    }
    let started = Instant::now();
    let timeout = Duration::from_secs_f64(repo.documentation.timeout_s);
    let deadline = started + timeout;
    let (mut child, tracker) = spawn_tracked(&mut command, token)?;
    let (read_tx, read_rx) = mpsc::channel();
    spawn_reader(
        "stdout",
        child.stdout.take().expect("piped stdout"),
        read_tx.clone(),
    );
    spawn_reader(
        "stderr",
        child.stderr.take().expect("piped stderr"),
        read_tx,
    );
    let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        Ok(None) => {
            terminate_process_group(child, deadline, tracker);
            return Ok(Some(DocumentationCheckFailure {
                timed_out: true,
                message: format!("timed out after {:.3}s", repo.documentation.timeout_s),
            }));
        }
        Err(error) => {
            terminate_process_group(child, deadline, tracker);
            return Err(error).context("cannot wait for documentation check");
        }
    };
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        match read_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(("stdout", result)) => stdout = Some(result?),
            Ok(("stderr", result)) => stderr = Some(result?),
            Ok((name, _)) => return Err(anyhow!("unknown documentation check stream {name}")),
            Err(RecvTimeoutError::Timeout) => {
                terminate_process_group(child, deadline, tracker);
                return Ok(Some(DocumentationCheckFailure {
                    timed_out: true,
                    message: "output did not drain before the deadline".into(),
                }));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("documentation check output readers disconnected"));
            }
        }
    }
    finish_process_group(tracker);
    if status.success() {
        return Ok(None);
    }
    let (stdout, stdout_truncated) = stdout.expect("stdout collected");
    let (stderr, stderr_truncated) = stderr.expect("stderr collected");
    let total = stdout.len() + stderr.len();
    let truncated = stdout_truncated || stderr_truncated;
    Ok(Some(DocumentationCheckFailure {
        timed_out: false,
        message: format!(
            "exit {}; captured {} output byte(s){}",
            status.code().unwrap_or(-1),
            total,
            if truncated { " (truncated)" } else { "" }
        ),
    }))
}

pub struct NativeDocumentationChecker;

impl DocumentationChecker for NativeDocumentationChecker {
    fn check(
        &self,
        repo: &RepoConfig,
        source_revision: &str,
        files: &[PublicationFile],
    ) -> Result<DocumentationCheck> {
        let mut failures = Vec::new();
        for (index, argv) in repo.documentation.commands.iter().enumerate() {
            if let Some(failure) = execute_command(repo, source_revision, files, argv)? {
                failures.push((index, failure));
            }
        }
        Ok(DocumentationCheck { failures })
    }
}
