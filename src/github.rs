use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const GH_TIMEOUT: Duration = Duration::from_secs(60);
const GH_OUTPUT_LIMIT: usize = 64 * 1024;

type GhRead = (&'static str, std::io::Result<Vec<u8>>);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DurablePrState {
    pub number: u64,
    pub url: String,
    pub repository: String,
    pub head: String,
    pub base: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnsurePullRequest<'a> {
    pub repository: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
    pub durable: Option<&'a DurablePrState>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    #[serde(rename = "state")]
    pub state: String,
    #[serde(rename = "headRefName")]
    pub head: String,
    #[serde(rename = "baseRefName")]
    pub base: String,
    #[serde(rename = "isDraft")]
    pub draft: bool,
    pub title: String,
    pub body: String,
    #[serde(rename = "headRefOid", default)]
    pub head_revision: Option<String>,
    #[serde(rename = "mergeCommit", default)]
    pub merge_commit: Option<MergeCommit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergeCommit {
    pub oid: String,
}

impl PullRequest {
    pub fn durable(&self, repository: &str) -> DurablePrState {
        DurablePrState {
            number: self.number,
            url: self.url.clone(),
            repository: repository.to_string(),
            head: self.head.clone(),
            base: self.base.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileAction {
    Created,
    Reused,
    Updated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReconciledPullRequest {
    pub action: ReconcileAction,
    pub pull_request: PullRequest,
    pub durable: DurablePrState,
}

#[derive(Clone, Debug)]
pub struct GhClient {
    program: PathBuf,
    cwd: PathBuf,
    timeout: Duration,
}

pub fn locale_branch(pattern: &str, locale: &str) -> Result<String> {
    if locale.is_empty()
        || !locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || locale.starts_with('.')
        || locale.ends_with('.')
        || locale.contains("..")
    {
        bail!("locale cannot form a stable Git branch: {locale:?}");
    }
    let branch = pattern.replace("{lang}", locale);
    if !pattern.contains("{lang}")
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch.bytes().any(|byte| {
            byte <= b' '
                || byte == 0x7f
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
    {
        bail!("branch pattern does not produce a valid stable branch: {branch:?}");
    }
    Ok(branch)
}

fn spawn_reader<R: Read + Send + 'static>(
    name: &'static str,
    mut pipe: R,
    sender: mpsc::Sender<GhRead>,
) {
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 8192];
        let result = (|| -> std::io::Result<Vec<u8>> {
            loop {
                let count = pipe.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                if output.len().saturating_add(count) > GH_OUTPUT_LIMIT {
                    return Err(std::io::Error::other(format!(
                        "gh {name} exceeded {GH_OUTPUT_LIMIT} bytes"
                    )));
                }
                output.extend_from_slice(&chunk[..count]);
            }
            Ok(output)
        })();
        let _ = sender.send((name, result));
    });
}

impl GhClient {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: PathBuf::from("gh"),
            cwd: cwd.into(),
            timeout: GH_TIMEOUT,
        }
    }

    pub fn with_program(cwd: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            cwd: cwd.into(),
            timeout: GH_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(&self.cwd)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run {}", self.program.display()))?;
        let (sender, receiver) = mpsc::channel();
        spawn_reader("stdout", child.stdout.take().unwrap(), sender.clone());
        spawn_reader("stderr", child.stderr.take().unwrap(), sender);

        let deadline = Instant::now() + self.timeout;
        let status = match child.wait_timeout(self.timeout)? {
            Some(status) => status,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("gh timed out after {:.0}s", self.timeout.as_secs_f64());
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
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("gh timed out while draining output");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("gh output reader disconnected");
                }
            }
        }
        let stdout = stdout.unwrap();
        let stderr = stderr.unwrap();
        if !status.success() {
            bail!("gh failed: {}", String::from_utf8_lossy(&stderr).trim());
        }
        String::from_utf8(stdout)
            .context("gh stdout was not UTF-8")
            .map(|output| output.trim().to_string())
    }

    pub fn pull_request(&self, repository: &str, selector: &str) -> Result<PullRequest> {
        let json = self.run([
            "pr",
            "view",
            selector,
            "--repo",
            repository,
            "--json",
            "number,url,state,headRefName,baseRefName,isDraft,title,body,headRefOid,mergeCommit",
        ])?;
        serde_json::from_str(&json).context("gh pr view returned invalid JSON")
    }

    fn list(&self, repository: &str, head: &str) -> Result<Vec<PullRequest>> {
        let json = self.run([
            "pr",
            "list",
            "--repo",
            repository,
            "--head",
            head,
            "--state",
            "open",
            "--limit",
            "2",
            "--json",
            "number,url,state,headRefName,baseRefName,isDraft,title,body,headRefOid,mergeCommit",
        ])?;
        serde_json::from_str(&json).context("gh pr list returned invalid JSON")
    }

    fn edit(&self, request: &EnsurePullRequest<'_>, pull: &PullRequest) -> Result<()> {
        let selector = pull.number.to_string();
        self.run([
            "pr",
            "edit",
            &selector,
            "--repo",
            request.repository,
            "--base",
            request.base,
            "--title",
            request.title,
            "--body",
            request.body,
        ])?;
        if pull.draft != request.draft {
            if request.draft {
                self.run([
                    "pr",
                    "ready",
                    &selector,
                    "--repo",
                    request.repository,
                    "--undo",
                ])?;
            } else {
                self.run(["pr", "ready", &selector, "--repo", request.repository])?;
            }
        }
        Ok(())
    }

    fn create(&self, request: &EnsurePullRequest<'_>) -> Result<PullRequest> {
        let mut args = vec![
            "pr",
            "create",
            "--repo",
            request.repository,
            "--head",
            request.head,
            "--base",
            request.base,
            "--title",
            request.title,
            "--body",
            request.body,
        ];
        if request.draft {
            args.push("--draft");
        }
        let output = self.run(args)?;
        let selector = output
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(str::trim)
            .ok_or_else(|| anyhow!("gh pr create did not return a pull request URL"))?;
        self.pull_request(request.repository, selector)
    }

    pub fn ensure_pull_request(
        &self,
        request: EnsurePullRequest<'_>,
    ) -> Result<ReconciledPullRequest> {
        if request.repository.trim().is_empty() {
            bail!("GitHub repository must not be empty");
        }
        if request.head.trim().is_empty() || request.base.trim().is_empty() {
            bail!("GitHub pull request head and base must not be empty");
        }

        if let Some(durable) = request.durable.filter(|durable| {
            durable.repository == request.repository
                && durable.head == request.head
                && durable.base == request.base
        }) {
            let pull = self.pull_request(request.repository, &durable.number.to_string())?;
            if pull.state.eq_ignore_ascii_case("merged") {
                return Ok(ReconciledPullRequest {
                    action: ReconcileAction::Reused,
                    durable: pull.durable(request.repository),
                    pull_request: pull,
                });
            }
        }

        let mut pulls = self.list(request.repository, request.head)?;
        pulls.retain(|pull| pull.state.eq_ignore_ascii_case("open") && pull.head == request.head);
        if pulls.len() > 1 {
            bail!(
                "multiple open pull requests exist for {}:{}",
                request.repository,
                request.head
            );
        }

        let (mut pull, created) = if let Some(pull) = pulls.pop() {
            (pull, false)
        } else if let Some(durable) = request.durable.filter(|durable| {
            durable.repository == request.repository
                && durable.head == request.head
                && durable.base == request.base
        }) {
            let pull = self.pull_request(request.repository, &durable.number.to_string())?;
            if pull.state.eq_ignore_ascii_case("open") && pull.head == request.head {
                (pull, false)
            } else {
                (self.create(&request)?, true)
            }
        } else {
            (self.create(&request)?, true)
        };

        let needs_update = pull.base != request.base
            || pull.title != request.title
            || pull.body != request.body
            || pull.draft != request.draft;
        let action = if needs_update {
            self.edit(&request, &pull)?;
            pull = self.pull_request(request.repository, &pull.number.to_string())?;
            ReconcileAction::Updated
        } else if created {
            ReconcileAction::Created
        } else {
            ReconcileAction::Reused
        };
        let durable = pull.durable(request.repository);
        Ok(ReconciledPullRequest {
            action,
            pull_request: pull,
            durable,
        })
    }
}
