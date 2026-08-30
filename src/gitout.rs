use crate::config::RepoConfig;
use crate::model::Published;
use crate::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use anyhow::{Context, Result, anyhow, bail};
use nix::unistd::{Pid, setpgid};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use wait_timeout::ChildExt;

const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PathChange {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub commit: String,
    pub tree: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateCommit {
    pub branch: String,
    pub commit: String,
    pub tree: String,
    pub previous_tip: Option<String>,
    pub source: SourceSnapshot,
    pub changes: Vec<PathChange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    kind: String,
}

type GitRead = (&'static str, std::io::Result<Vec<u8>>);

struct Git<'a> {
    root: &'a Path,
    program: PathBuf,
    timeout: Duration,
}

fn spawn_git_reader<R: Read + Send + 'static>(
    name: &'static str,
    mut pipe: R,
    sender: mpsc::Sender<GitRead>,
) {
    thread::spawn(move || {
        let mut tail = Vec::new();
        let mut chunk = [0_u8; 8192];
        let result = (|| -> std::io::Result<Vec<u8>> {
            loop {
                let count = pipe.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                let excess = tail
                    .len()
                    .saturating_add(count)
                    .saturating_sub(GIT_OUTPUT_LIMIT);
                if excess > 0 {
                    tail.drain(..excess);
                }
                tail.extend_from_slice(&chunk[..count]);
            }
            Ok(tail)
        })();
        let _ = sender.send((name, result));
    });
}

impl<'a> Git<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            program: PathBuf::from("git"),
            timeout: GIT_TIMEOUT,
        }
    }

    fn run<I, S>(&self, args: I, index: Option<&Path>, check: bool) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        enable_subreaper()?;
        let started = Instant::now();
        let token = process_token();
        let mut cmd = wrapped_command(&self.program)?;
        cmd.args(args)
            .current_dir(self.root)
            .env("FANI_PROCESS_TOKEN", &token)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        unsafe {
            cmd.pre_exec(|| {
                setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::other)
            });
        }
        let (mut child, tracker) = spawn_tracked(&mut cmd, token).context("cannot run git")?;
        let (sender, receiver) = mpsc::channel();
        spawn_git_reader("stdout", child.stdout.take().unwrap(), sender.clone());
        spawn_git_reader("stderr", child.stderr.take().unwrap(), sender);

        let deadline = started + self.timeout;
        let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now()))? {
            Some(status) => status,
            None => {
                terminate_process_group(
                    child,
                    Instant::now() + Duration::from_millis(250),
                    tracker,
                );
                bail!("git timed out after {:.0}s", self.timeout.as_secs_f64());
            }
        };
        let mut stdout = None;
        let mut stderr = None;
        while stdout.is_none() || stderr.is_none() {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(("stdout", result)) => stdout = Some(result?),
                Ok(("stderr", result)) => stderr = Some(result?),
                Ok(_) => unreachable!(),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    terminate_process_group(
                        child,
                        Instant::now() + Duration::from_millis(250),
                        tracker,
                    );
                    bail!(
                        "git timed out after {:.0}s draining output",
                        self.timeout.as_secs_f64()
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("git output reader disconnected");
                }
            }
        }
        finish_process_group(tracker);
        let stdout = stdout.unwrap();
        let stderr = stderr.unwrap();
        if check && !status.success() {
            bail!("git failed: {}", String::from_utf8_lossy(&stderr).trim());
        }
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }

    fn rev(&self, reference: &str) -> Result<Option<String>> {
        let value = self.run(
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
            None,
            false,
        )?;
        Ok((!value.is_empty()).then_some(value))
    }

    fn snapshot(&self, source_ref: &str) -> Result<SourceSnapshot> {
        let commit = self
            .rev(source_ref)?
            .ok_or_else(|| anyhow!("source ref {source_ref:?} does not resolve to a commit"))?;
        let tree = self.run(["rev-parse", &format!("{commit}^{{tree}}")], None, true)?;
        Ok(SourceSnapshot { commit, tree })
    }

    fn tree_entry(&self, treeish: &str, path: &str) -> Result<Option<TreeEntry>> {
        let output = self.run(["ls-tree", "-z", treeish, "--", path], None, true)?;
        if output.is_empty() {
            return Ok(None);
        }
        let metadata = output
            .split_once('\t')
            .map(|(metadata, _)| metadata)
            .ok_or_else(|| anyhow!("unexpected git ls-tree output for {path:?}"))?;
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default().to_string();
        if mode.is_empty() || fields.next().is_none() || fields.next().is_some() {
            bail!("unexpected git ls-tree metadata for {path:?}");
        }
        Ok(Some(TreeEntry { kind }))
    }

    fn remote_tip(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        let reference = format!("refs/heads/{branch}");
        let output = self.run(["ls-remote", "--refs", remote, &reference], None, true)?;
        if output.is_empty() {
            return Ok(None);
        }
        let mut lines = output.lines();
        let line = lines.next().unwrap();
        if lines.next().is_some() {
            bail!("remote {remote:?} returned multiple tips for {reference}");
        }
        let (oid, found_ref) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| anyhow!("unexpected git ls-remote output"))?;
        if found_ref.trim() != reference {
            bail!("remote {remote:?} returned an unexpected ref {found_ref:?}");
        }
        Ok(Some(oid.to_string()))
    }
}

fn valid_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn checked_path(path: &str) -> Result<String> {
    let candidate = Path::new(path);
    if !valid_relative(candidate)
        || path.contains('\0')
        || path == ".git"
        || path.starts_with(".git/")
    {
        bail!("publication path must stay inside the repository: {path:?}");
    }
    Ok(candidate.to_string_lossy().into_owned())
}

fn reported_paths(written: &[Value]) -> Result<Vec<String>> {
    let mut paths = BTreeMap::<String, ()>::new();
    for item in written {
        let Some(path) = item
            .get("target")
            .or_else(|| item.get("path"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        paths.insert(checked_path(path)?, ());
    }
    Ok(paths.into_keys().collect())
}

pub fn classify_changes(
    repo: &RepoConfig,
    base_treeish: &str,
    written: &[Value],
) -> Result<Vec<PathChange>> {
    let git = Git::new(&repo.path);
    let mut changes = Vec::new();
    for path in reported_paths(written)? {
        let entry = git.tree_entry(base_treeish, &path)?;
        if entry.as_ref().is_some_and(|entry| entry.kind != "blob") {
            bail!("publication path {path:?} is not a blob in the candidate tree");
        }
        let disk = repo.path.join(&path);
        let kind = if disk.exists() {
            let metadata = fs::symlink_metadata(&disk)
                .with_context(|| format!("cannot inspect publication path {}", disk.display()))?;
            if !metadata.file_type().is_file() {
                bail!(
                    "publication path must be a regular file: {}",
                    disk.display()
                );
            }
            if entry.is_some() {
                ChangeKind::Modify
            } else {
                ChangeKind::Add
            }
        } else if entry.is_some() {
            ChangeKind::Delete
        } else {
            bail!(
                "reported publication path does not exist in the worktree or candidate tree: {path}"
            );
        };
        changes.push(PathChange { path, kind });
    }
    Ok(changes)
}

pub fn create_candidate(
    repo_path: &Path,
    source_ref: &str,
    branch: &str,
    changes: &[PathChange],
    message: &str,
) -> Result<CandidateCommit> {
    let git = Git::new(repo_path);
    if git.run(["rev-parse", "--is-inside-work-tree"], None, false)? != "true" {
        bail!("{} is not a git worktree", repo_path.display());
    }
    git.run(["check-ref-format", "--branch", branch], None, true)?;

    let source = git.snapshot(source_ref)?;
    let candidate_ref = format!("refs/heads/{branch}");
    let previous_tip = git.rev(&candidate_ref)?;
    let base = source.commit.as_str();
    let base_tree = source.tree.clone();
    let tmp = tempdir().context("cannot create temporary Git index")?;
    let index = tmp.path().join("index");
    git.run(["read-tree", &base_tree], Some(&index), true)?;

    let mut seen = BTreeMap::new();
    for change in changes {
        let path = checked_path(&change.path)?;
        if seen.insert(path.clone(), change.kind.clone()).is_some() {
            bail!("duplicate publication path {path:?}");
        }
        let entry = git.tree_entry(&base_tree, &path)?;
        match (&change.kind, entry.as_ref()) {
            (ChangeKind::Add, Some(_)) => {
                bail!("add path already exists in candidate tree: {path}")
            }
            (ChangeKind::Modify | ChangeKind::Delete, None) => {
                bail!(
                    "{} path is absent from candidate tree: {path}",
                    match change.kind {
                        ChangeKind::Modify => "modify",
                        ChangeKind::Delete => "delete",
                        ChangeKind::Add => unreachable!(),
                    }
                )
            }
            (_, Some(entry)) if entry.kind != "blob" => {
                bail!("publication path is not a blob in candidate tree: {path}")
            }
            _ => {}
        }

        match change.kind {
            ChangeKind::Delete => {
                git.run(
                    ["update-index", "--force-remove", "--", &path],
                    Some(&index),
                    true,
                )?;
            }
            ChangeKind::Add | ChangeKind::Modify => {
                let disk = repo_path.join(&path);
                let metadata = fs::symlink_metadata(&disk).with_context(|| {
                    format!("cannot inspect publication path {}", disk.display())
                })?;
                if !metadata.file_type().is_file() {
                    bail!(
                        "publication path must be a regular file: {}",
                        disk.display()
                    );
                }
                let oid = git.run(["hash-object", "-w", "--", &path], None, true)?;
                let mode = if metadata.permissions().mode() & 0o111 == 0 {
                    "100644"
                } else {
                    "100755"
                };
                git.run(
                    ["update-index", "--add", "--cacheinfo", mode, &oid, &path],
                    Some(&index),
                    true,
                )?;
            }
        }
    }

    let tree = git.run(["write-tree"], Some(&index), true)?;
    let commit = if tree == base_tree {
        base.to_string()
    } else {
        git.run(
            ["commit-tree", &tree, "-p", base, "-m", message],
            None,
            true,
        )?
    };
    if previous_tip.as_deref() != Some(commit.as_str()) {
        let expected = previous_tip.as_deref().unwrap_or("");
        git.run(
            ["update-ref", &candidate_ref, &commit, expected],
            None,
            true,
        )
        .with_context(|| format!("candidate ref {candidate_ref} changed concurrently"))?;
    }

    Ok(CandidateCommit {
        branch: branch.to_string(),
        commit,
        tree,
        previous_tip,
        source,
        changes: changes.to_vec(),
    })
}

pub fn push_candidate(
    repo_path: &Path,
    remote: &str,
    candidate: &CandidateCommit,
    expected_remote_tip: Option<&str>,
) -> Result<()> {
    let git = Git::new(repo_path);
    let candidate_ref = format!("refs/heads/{}", candidate.branch);
    if git.rev(&candidate_ref)?.as_deref() != Some(candidate.commit.as_str()) {
        bail!(
            "candidate commit {} is no longer the tip of {candidate_ref}",
            candidate.commit
        );
    }
    let remote_ref = format!("refs/heads/{}", candidate.branch);
    let observed = git.remote_tip(remote, &candidate.branch)?;
    if observed.as_deref() != expected_remote_tip {
        bail!(
            "remote ref {remote_ref} changed: expected {}, found {}",
            expected_remote_tip.unwrap_or("<absent>"),
            observed.as_deref().unwrap_or("<absent>")
        );
    }
    let lease = format!(
        "--force-with-lease={remote_ref}:{}",
        expected_remote_tip.unwrap_or("")
    );
    let refspec = format!("{candidate_ref}:{remote_ref}");
    git.run(["push", &lease, remote, &refspec], None, true)?;
    Ok(())
}

pub fn allowed_paths(written: &[Value]) -> Result<Vec<String>> {
    reported_paths(written)
}

pub fn publish(repo: &RepoConfig, lang: &str, written: &[Value]) -> Result<Published> {
    let branch = repo.publish.branch.replace("{lang}", lang);
    let mut result = Published {
        branch: branch.clone(),
        ..Published::default()
    };
    if !repo.publish.enabled {
        result.skipped = "publication is disabled for this repo".into();
        return Ok(result);
    }
    if written.is_empty() {
        result.skipped = "nothing was written".into();
        return Ok(result);
    }

    let git = Git::new(&repo.path);
    let source = git.snapshot(&repo.publish.source_ref)?;
    let candidate_ref = format!("refs/heads/{branch}");
    let _previous_tip = git.rev(&candidate_ref)?;
    let changes = classify_changes(repo, &source.tree, written)?;
    result.paths = changes.iter().map(|change| change.path.clone()).collect();
    if changes.is_empty() {
        result.skipped = "no allowlisted publication paths were reported".into();
        return Ok(result);
    }

    let message = format!("i18n({lang}): update {} translated file(s)", changes.len());
    let candidate = create_candidate(&repo.path, &source.commit, &branch, &changes, &message)?;
    result.commit = candidate.commit.clone();
    if candidate.previous_tip.as_deref() == Some(candidate.commit.as_str()) {
        result.skipped = "the translations are already committed".into();
    }
    if repo.publish.push {
        let expected_remote = git.remote_tip(&repo.publish.remote, &branch)?;
        match push_candidate(
            &repo.path,
            &repo.publish.remote,
            &candidate,
            expected_remote.as_deref(),
        ) {
            Ok(()) => result.pushed = true,
            Err(error) => {
                result.error = format!("could not push commit {}: {error}", result.commit);
            }
        }
    }
    Ok(result)
}

pub fn publish_pending(repo: &RepoConfig, lang: &str, commit: &str) -> Result<Published> {
    let branch = repo.publish.branch.replace("{lang}", lang);
    let mut result = Published {
        branch: branch.clone(),
        commit: commit.to_string(),
        skipped: "retrying a previously failed push".into(),
        ..Published::default()
    };
    if !repo.publish.enabled || !repo.publish.push {
        return Ok(result);
    }
    let git = Git::new(&repo.path);
    let candidate_ref = format!("refs/heads/{branch}");
    if git.rev(&candidate_ref)?.as_deref() != Some(commit) {
        bail!("pending commit {commit} is no longer the tip of {candidate_ref}");
    }
    let source = git.snapshot(&repo.publish.source_ref)?;
    let tree = git.run(["rev-parse", &format!("{commit}^{{tree}}")], None, true)?;
    let candidate = CandidateCommit {
        branch: branch.clone(),
        commit: commit.to_string(),
        tree,
        previous_tip: Some(commit.to_string()),
        source,
        changes: Vec::new(),
    };
    let expected_remote = git.remote_tip(&repo.publish.remote, &branch)?;
    match push_candidate(
        &repo.path,
        &repo.publish.remote,
        &candidate,
        expected_remote.as_deref(),
    ) {
        Ok(()) => result.pushed = true,
        Err(error) => result.error = format!("could not push commit {commit}: {error}"),
    }
    Ok(result)
}
