use crate::config::RepoConfig;
use crate::model::Published;
use crate::process::{
    enable_subreaper, finish_process_group, process_token, spawn_tracked, terminate_process_group,
    wrapped_command,
};
use anyhow::{Context, Result, anyhow};
use nix::unistd::{Pid, setpgid};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::Read;
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

struct Git<'a> {
    root: &'a Path,
    program: PathBuf,
    timeout: Duration,
}

type GitRead = (&'static str, std::io::Result<Vec<u8>>);

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
                if count >= GIT_OUTPUT_LIMIT {
                    tail.clear();
                    tail.extend_from_slice(&chunk[count - GIT_OUTPUT_LIMIT..count]);
                } else {
                    let excess = tail
                        .len()
                        .saturating_add(count)
                        .saturating_sub(GIT_OUTPUT_LIMIT);
                    if excess > 0 {
                        tail.drain(..excess);
                    }
                    tail.extend_from_slice(&chunk[..count]);
                }
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
                return Err(anyhow!(
                    "git timed out after {:.0}s",
                    self.timeout.as_secs_f64()
                ));
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
                    return Err(anyhow!(
                        "git timed out after {:.0}s draining output",
                        self.timeout.as_secs_f64()
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("git output reader disconnected"));
                }
            }
        }
        let stdout = stdout.unwrap();
        let stderr = stderr.unwrap();
        finish_process_group(tracker);
        if check && !status.success() {
            return Err(anyhow!(
                "git failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }

    fn rev(&self, reference: &str) -> Result<String> {
        self.run(
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
            None,
            false,
        )
    }

    fn is_repo(&self) -> bool {
        self.run(["rev-parse", "--is-inside-work-tree"], None, false)
            .is_ok_and(|x| x == "true")
    }

    fn commit(&self, paths: &[String], branch: &str, message: &str) -> Result<String> {
        let tip = self.rev(&format!("refs/heads/{branch}"))?;
        let base = if tip.is_empty() {
            self.rev("HEAD")?
        } else {
            tip.clone()
        };
        let tmp = tempdir()?;
        let index = tmp.path().join("index");
        if !base.is_empty() {
            self.run(["read-tree", &base], Some(&index), true)?;
        }
        let mut update = vec!["update-index".to_string(), "--add".into(), "--".into()];
        update.extend(paths.iter().cloned());
        self.run(update, Some(&index), true)?;
        let tree = self.run(["write-tree"], Some(&index), true)?;
        if !tip.is_empty() {
            let old_tree = self.run(["rev-parse", &format!("{tip}^{{tree}}")], None, true)?;
            if old_tree == tree {
                return Ok(String::new());
            }
        }
        let mut commit_args = vec!["commit-tree".to_string(), tree];
        if !base.is_empty() {
            commit_args.extend(["-p".into(), base]);
        }
        commit_args.extend(["-m".into(), message.into()]);
        let sha = self.run(commit_args, None, true)?;
        let expected = if tip.is_empty() { "0".repeat(40) } else { tip };
        self.run(
            [
                "update-ref",
                &format!("refs/heads/{branch}"),
                &sha,
                &expected,
            ],
            None,
            true,
        )?;
        Ok(sha)
    }
}

fn valid_relative(rel: &Path) -> bool {
    !rel.is_absolute()
        && !rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

pub fn allowed_paths(repo: &RepoConfig, written: &[Value]) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for item in written {
        let rel = item
            .get("target")
            .or_else(|| item.get("path"))
            .and_then(Value::as_str);
        let Some(rel) = rel else {
            continue;
        };
        let path = PathBuf::from(rel);
        if !valid_relative(&path) {
            return Err(anyhow!(
                "apply reported a path outside the repository: {rel}"
            ));
        }
        if repo.path.join(&path).is_file() {
            paths.push(path.display().to_string());
        }
    }
    // The external skill owns these JSON files. fani.db is intentionally excluded:
    // it contains local scheduler history and a live lock table, not translation memory.
    for name in ["state.json", "glossary.json", "style.json"] {
        let rel = format!("{}/{name}", repo.state_dir.trim_end_matches('/'));
        if repo.path.join(&rel).is_file() {
            paths.push(rel);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub fn publish(repo: &RepoConfig, lang: &str, written: &[Value]) -> Result<Published> {
    let branch = repo.branch.replace("{lang}", lang);
    let mut result = Published {
        branch: branch.clone(),
        ..Published::default()
    };
    if !repo.commit {
        result.skipped = "commit is disabled for this repo".into();
        return Ok(result);
    }
    let git = Git::new(&repo.path);
    if !git.is_repo() {
        return Err(anyhow!(
            "{} is not a git repository; set commit = false to skip",
            repo.path.display()
        ));
    }
    if written.is_empty() {
        result.skipped = "nothing was written".into();
        return Ok(result);
    }
    result.paths = allowed_paths(repo, written)?;
    if result.paths.is_empty() {
        result.skipped = "none of the written files exist on disk".into();
        return Ok(result);
    }
    let message = format!("i18n({lang}): update {} translated file(s)", written.len());
    result.commit = git.commit(&result.paths, &branch, &message)?;
    if result.commit.is_empty() {
        result.commit = git.rev(&format!("refs/heads/{branch}"))?;
        result.skipped = "the translations are already committed".into();
    }
    if repo.push && !result.commit.is_empty() {
        match git.run(
            [
                "push",
                &repo.remote,
                &format!("refs/heads/{branch}:refs/heads/{branch}"),
            ],
            None,
            true,
        ) {
            Ok(_) => result.pushed = true,
            Err(error) => {
                result.error = format!("could not push commit {}: {error}", result.commit)
            }
        }
    }
    Ok(result)
}

pub fn publish_pending(repo: &RepoConfig, lang: &str, commit: &str) -> Result<Published> {
    let branch = repo.branch.replace("{lang}", lang);
    let mut result = Published {
        branch: branch.clone(),
        commit: commit.to_string(),
        skipped: "retrying a previously failed push".into(),
        ..Published::default()
    };
    if !repo.commit || !repo.push {
        return Ok(result);
    }
    let git = Git::new(&repo.path);
    if !git.is_repo() {
        return Err(anyhow!(
            "{} is not a git repository; cannot retry pending publication",
            repo.path.display()
        ));
    }
    let tip = git.rev(&format!("refs/heads/{branch}"))?;
    if tip != commit {
        return Err(anyhow!(
            "pending commit {commit} is no longer the tip of refs/heads/{branch}"
        ));
    }
    match git.run(
        [
            "push",
            &repo.remote,
            &format!("refs/heads/{branch}:refs/heads/{branch}"),
        ],
        None,
        true,
    ) {
        Ok(_) => result.pushed = true,
        Err(error) => result.error = format!("could not push commit {commit}: {error}"),
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn setup() -> (tempfile::TempDir, RepoConfig) {
        let tmp = tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(tmp.path(), &["config", "user.email", "t@e"]);
        git(tmp.path(), &["config", "user.name", "t"]);
        fs::create_dir(tmp.path().join("docs")).unwrap();
        fs::write(tmp.path().join("docs/guide.md"), "source\n").unwrap();
        git(tmp.path(), &["add", "-A"]);
        git(tmp.path(), &["commit", "-qm", "initial"]);
        let repo = RepoConfig {
            path: tmp.path().to_path_buf(),
            languages: vec!["zh-CN".into()],
            paths: vec![],
            exclude: vec![],
            state_dir: ".fani-state".into(),
            max_tasks: 40,
            repair_budget: 2,
            full_retranslate_guard: 30,
            branch: "i18n/{lang}".into(),
            commit: true,
            push: false,
            remote: "origin".into(),
            stages: Default::default(),
        };
        (tmp, repo)
    }

    #[test]
    fn git_subprocess_timeout_is_bounded() {
        let tmp = tempdir().unwrap();
        let script = tmp.path().join("blocking-git");
        let pid_file = tmp.path().join("git.pid");
        let escaped_pid_file = tmp.path().join("git-escaped.pid");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap '' TERM\necho $$ > '{}'\npython3 -c \"import os,time; pid=os.fork(); os._exit(0) if pid else None; os.setsid(); open('{}','w').write(str(os.getpid())); time.sleep(30)\" &\nsleep 30\n",
                pid_file.display(),
                escaped_pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let git = Git {
            root: tmp.path(),
            program: script,
            timeout: Duration::from_millis(100),
        };
        let started = Instant::now();
        let error = git.run(["status"], None, true).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        let pid: i32 = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let escaped_pid: i32 = fs::read_to_string(escaped_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for tracked_pid in [pid, escaped_pid] {
            let reaped = (0..100).any(|_| {
                if !Path::new(&format!("/proc/{tracked_pid}")).exists() {
                    true
                } else {
                    thread::sleep(Duration::from_millis(10));
                    false
                }
            });
            assert!(reaped, "Git helper {tracked_pid} survived timeout");
        }
    }

    #[test]
    fn failed_push_preserves_commit_and_no_change_retry_publishes_it() {
        let (tmp, mut repo) = setup();
        let remote = tmp.path().join("remote.git");
        git(
            tmp.path(),
            &["init", "--bare", "-q", remote.to_str().unwrap()],
        );
        git(
            tmp.path(),
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let hook = remote.join("hooks/pre-receive");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        repo.push = true;
        fs::write(tmp.path().join("docs/guide.zh-CN.md"), "译文\n").unwrap();

        let first = publish(&repo, "zh-CN", &[json!({"path":"docs/guide.zh-CN.md"})]).unwrap();
        assert!(!first.commit.is_empty());
        assert!(!first.pushed);
        assert!(first.error.contains("could not push"));
        assert_eq!(git(tmp.path(), &["rev-parse", "i18n/zh-CN"]), first.commit);
        assert!(
            !Command::new("git")
                .args(["rev-parse", "--verify", "refs/heads/i18n/zh-CN"])
                .current_dir(&remote)
                .output()
                .unwrap()
                .status
                .success()
        );

        fs::remove_file(hook).unwrap();
        let retry = publish_pending(&repo, "zh-CN", &first.commit).unwrap();
        assert_eq!(retry.commit, first.commit);
        assert!(retry.pushed);
        assert!(retry.error.is_empty());
        assert_eq!(
            git(&remote, &["rev-parse", "refs/heads/i18n/zh-CN"]),
            first.commit
        );
    }

    #[test]
    fn commit_is_isolated_and_database_is_excluded() {
        let (tmp, repo) = setup();
        fs::write(tmp.path().join("docs/guide.zh-CN.md"), "译文\n").unwrap();
        fs::create_dir(tmp.path().join(".fani-state")).unwrap();
        fs::write(tmp.path().join(".fani-state/state.json"), "{}").unwrap();
        fs::write(tmp.path().join(".fani-state/fani.db"), "local-db").unwrap();
        fs::write(tmp.path().join("scratch.txt"), "mine").unwrap();
        let head = git(tmp.path(), &["rev-parse", "HEAD"]);
        let out = publish(&repo, "zh-CN", &[json!({"path":"docs/guide.zh-CN.md"})]).unwrap();
        assert!(!out.commit.is_empty());
        assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(tmp.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        let files = git(tmp.path(), &["ls-tree", "-r", "--name-only", "i18n/zh-CN"]);
        assert!(files.contains("docs/guide.zh-CN.md"));
        assert!(files.contains(".fani-state/state.json"));
        assert!(!files.contains("fani.db"));
        assert!(!files.contains("scratch.txt"));
    }
}
