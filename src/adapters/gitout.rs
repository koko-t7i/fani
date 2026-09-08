use crate::adapters::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use crate::application::ports::{GitPublisher, PreparedPublication, PublicationFile};
use crate::application::settings::RepoConfig;
use crate::domain::model::Published;
use crate::domain::model::SourceDocument;
use anyhow::{Context, Result, anyhow, bail};
use nix::unistd::{Pid, setpgid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
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
        self.run_with_date(args, index, check, None)
    }

    fn run_with_date<I, S>(
        &self,
        args: I,
        index: Option<&Path>,
        check: bool,
        commit_date: Option<&str>,
    ) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_options(args, index, check, commit_date, None)
    }

    fn run_with_input<I, S>(&self, args: I, input: &[u8], check: bool) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_options(args, None, check, None, Some(input))
    }

    fn run_with_options<I, S>(
        &self,
        args: I,
        index: Option<&Path>,
        check: bool,
        commit_date: Option<&str>,
        input: Option<&[u8]>,
    ) -> Result<String>
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
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        if let Some(commit_date) = commit_date {
            cmd.env("GIT_AUTHOR_DATE", commit_date)
                .env("GIT_COMMITTER_DATE", commit_date);
        }
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        unsafe {
            cmd.pre_exec(|| {
                setpgid(Pid::from_raw(0), Pid::from_raw(0)).map_err(std::io::Error::other)
            });
        }
        let (mut child, tracker) = spawn_tracked(&mut cmd, token).context("cannot run git")?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("piped Git stdin");
            stdin.write_all(input)?;
        }
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

pub fn create_candidate(
    repo_path: &Path,
    source_ref: &str,
    branch: &str,
    changes: &[PathChange],
    message: &str,
) -> Result<CandidateCommit> {
    create_candidate_inner(repo_path, source_ref, branch, changes, message, None)
}

fn create_candidate_inner(
    repo_path: &Path,
    source_ref: &str,
    branch: &str,
    changes: &[PathChange],
    message: &str,
    contents: Option<&BTreeMap<String, Vec<u8>>>,
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
    let commit_date = git.run(["show", "-s", "--format=%cI", base], None, true)?;
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
                let (oid, mode) = if let Some(contents) = contents {
                    let content = contents
                        .get(&path)
                        .ok_or_else(|| anyhow!("durable publication content is missing {path}"))?;
                    (
                        git.run_with_input(["hash-object", "-w", "--stdin"], content, true)?,
                        "100644",
                    )
                } else {
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
                    (oid, mode)
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
        git.run_with_date(
            ["commit-tree", &tree, "-p", base, "-m", message],
            None,
            true,
            Some(&commit_date),
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

pub fn create_candidate_from_contents(
    repo_path: &Path,
    source_ref: &str,
    branch: &str,
    files: &[(String, Vec<u8>)],
    message: &str,
) -> Result<CandidateCommit> {
    let git = Git::new(repo_path);
    let source = git.snapshot(source_ref)?;
    let mut contents = BTreeMap::new();
    let mut changes = Vec::with_capacity(files.len());
    for (path, content) in files {
        let path = checked_path(path)?;
        if contents.insert(path.clone(), content.clone()).is_some() {
            bail!("duplicate publication path {path:?}");
        }
        let kind = if git.tree_entry(&source.tree, &path)?.is_some() {
            ChangeKind::Modify
        } else {
            ChangeKind::Add
        };
        changes.push(PathChange { path, kind });
    }
    create_candidate_inner(
        repo_path,
        &source.commit,
        branch,
        &changes,
        message,
        Some(&contents),
    )
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

pub fn remote_branch_tip(repo_path: &Path, remote: &str, branch: &str) -> Result<Option<String>> {
    Git::new(repo_path).remote_tip(remote, branch)
}

pub fn publish_pending_with_expected(
    repo: &RepoConfig,
    lang: &str,
    commit: &str,
    expected_remote_tip: Option<&str>,
) -> Result<Published> {
    let branch = repo.publish.branch.replace("{lang}", lang);
    let mut result = Published {
        branch: branch.clone(),
        commit: commit.to_string(),
        skipped: "retrying a durable publication candidate".into(),
        ..Published::default()
    };
    if !repo.publish.enabled || !repo.publish.push {
        return Ok(result);
    }
    let git = Git::new(&repo.path);
    let observed = git.remote_tip(&repo.publish.remote, &branch)?;
    if observed.as_deref() == Some(commit) {
        result.pushed = true;
        result.skipped = "the durable publication commit is already remote".into();
        return Ok(result);
    }
    if observed.as_deref() != expected_remote_tip {
        bail!(
            "remote publication branch changed: expected {}, found {}",
            expected_remote_tip.unwrap_or("<absent>"),
            observed.as_deref().unwrap_or("<absent>")
        );
    }
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
    match push_candidate(
        &repo.path,
        &repo.publish.remote,
        &candidate,
        expected_remote_tip,
    ) {
        Ok(()) => result.pushed = true,
        Err(error) => result.error = format!("could not push commit {commit}: {error}"),
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeGitPublisher;

impl GitPublisher for NativeGitPublisher {
    fn resolve_source_revision(&self, repo: &RepoConfig) -> Result<String> {
        crate::adapters::source::resolve_source_revision(repo)
    }

    fn discover(
        &self,
        repo: &RepoConfig,
        source_revision: &str,
    ) -> Result<Vec<crate::domain::model::SourceDocument>> {
        crate::adapters::source::discover(repo, source_revision)
    }

    fn branch(&self, repo: &RepoConfig, language: &str) -> Result<String> {
        crate::adapters::github::locale_branch(&repo.publish.branch, language)
    }

    fn prepare(
        &self,
        repo: &RepoConfig,
        language: &str,
        source_revision: &str,
        files: &[PublicationFile],
    ) -> Result<PreparedPublication> {
        let branch = self.branch(repo, language)?;
        let message = format!(
            "i18n({language}): update {} translated file(s)",
            files.len()
        );
        let durable_files: Vec<_> = files
            .iter()
            .map(|file| (file.path.clone(), file.content.clone()))
            .collect();
        let candidate = create_candidate_from_contents(
            &repo.path,
            source_revision,
            &branch,
            &durable_files,
            &message,
        )?;
        let mut published = Published {
            branch,
            commit: candidate.commit.clone(),
            paths: candidate
                .changes
                .iter()
                .map(|change| change.path.clone())
                .collect(),
            ..Published::default()
        };
        if candidate.previous_tip.as_deref() == Some(candidate.commit.as_str()) {
            published.skipped = "the translations are already committed".into();
        }
        let expected_remote_tip = if repo.publish.push {
            remote_branch_tip(&repo.path, &repo.publish.remote, &published.branch)?
        } else {
            None
        };
        Ok(PreparedPublication {
            published,
            expected_remote_tip,
        })
    }

    fn publish_pending(
        &self,
        repo: &RepoConfig,
        language: &str,
        commit: &str,
        expected_remote_tip: Option<&str>,
    ) -> Result<Published> {
        publish_pending_with_expected(repo, language, commit, expected_remote_tip)
    }
}

impl crate::application::ports::SourceReader for NativeGitPublisher {
    fn resolve_source_revision(&self, repo: &RepoConfig) -> Result<String> {
        <Self as GitPublisher>::resolve_source_revision(self, repo)
    }

    fn discover(&self, repo: &RepoConfig, source_revision: &str) -> Result<Vec<SourceDocument>> {
        <Self as GitPublisher>::discover(self, repo, source_revision)
    }
}
