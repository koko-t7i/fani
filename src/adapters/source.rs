use crate::application::settings::RepoConfig;
use crate::domain::model::SourceDocument;
use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

const GIT_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
type GitRead = (&'static str, std::io::Result<Vec<u8>>);

fn hash(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

fn read_bounded<R: Read + Send + 'static>(
    name: &'static str,
    mut pipe: R,
    sender: mpsc::Sender<GitRead>,
) {
    thread::spawn(move || {
        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut output = Vec::new();
            let mut chunk = [0_u8; 8192];
            loop {
                let count = pipe.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                if output.len().saturating_add(count) > GIT_OUTPUT_LIMIT {
                    return Err(std::io::Error::other(format!(
                        "git {name} exceeded {GIT_OUTPUT_LIMIT} bytes"
                    )));
                }
                output.extend_from_slice(&chunk[..count]);
            }
            Ok(output)
        })();
        let _ = sender.send((name, result));
    });
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot execute git in {}", root.display()))?;
    let (sender, receiver) = mpsc::channel();
    read_bounded("stdout", child.stdout.take().unwrap(), sender.clone());
    read_bounded("stderr", child.stderr.take().unwrap(), sender);
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = match child.wait_timeout(GIT_TIMEOUT)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("git {:?} timed out", args));
        }
    };
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(("stdout", result)) => stdout = Some(result?),
            Ok(("stderr", result)) => stderr = Some(result?),
            Ok(_) => unreachable!(),
            Err(error) => return Err(anyhow!("git output reader failed: {error}")),
        }
    }
    let stdout = stdout.unwrap();
    let stderr = stderr.unwrap();
    if !status.success() {
        return Err(anyhow!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(stdout)
}

fn globset(patterns: &[String], defaults: &[&str]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    if patterns.is_empty() {
        for pattern in defaults {
            builder.add(Glob::new(pattern)?);
        }
    } else {
        for pattern in patterns {
            builder.add(Glob::new(pattern).with_context(|| format!("invalid glob {pattern:?}"))?);
        }
    }
    Ok(builder.build()?)
}

pub fn resolve_source_revision(repo: &RepoConfig) -> Result<String> {
    let reference = format!("{}^{{commit}}", repo.publish.source_ref);
    let bytes = git(&repo.path, &["rev-parse", "--verify", &reference])?;
    let revision = String::from_utf8(bytes)?.trim().to_owned();
    if revision.len() < 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!("git returned invalid source revision {revision:?}"));
    }
    Ok(revision)
}

pub fn discover(repo: &RepoConfig, source_revision: &str) -> Result<Vec<SourceDocument>> {
    let include = globset(&repo.include, &["**/*.md", "*.md"])?;
    let exclude = globset(&repo.exclude, &[])?;
    let listing = git(
        &repo.path,
        &["ls-tree", "-r", "--name-only", "-z", source_revision],
    )?;
    let repository = repo
        .path
        .canonicalize()
        .unwrap_or_else(|_| repo.path.clone())
        .display()
        .to_string();
    let mut documents = Vec::new();
    for raw in listing
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        let path = String::from_utf8(raw.to_vec()).context("Git path is not UTF-8")?;
        if !include.is_match(&path) || exclude.is_match(&path) {
            continue;
        }
        let object = format!("{source_revision}:{path}");
        let bytes = git(&repo.path, &["show", &object])?;
        let content_hash = hash(&[&bytes]);
        documents.push(SourceDocument {
            repository: repository.clone(),
            source_revision: source_revision.to_owned(),
            path,
            bytes,
            content_hash,
        });
    }
    documents.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(documents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::settings::{
        DocumentationConfig, GithubConfig, PublishConfig, QualityConfig,
    };
    use std::fs;
    use tempfile::tempdir;

    fn command(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn discovery_reads_only_the_fixed_revision() {
        let tmp = tempdir().unwrap();
        command(tmp.path(), &["init", "-q", "-b", "main"]);
        command(
            tmp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        command(tmp.path(), &["config", "user.name", "Test"]);
        fs::create_dir(tmp.path().join("docs")).unwrap();
        fs::write(tmp.path().join("docs/a.md"), "old\n").unwrap();
        command(tmp.path(), &["add", "."]);
        command(tmp.path(), &["commit", "-qm", "initial"]);
        let repo = RepoConfig {
            path: tmp.path().into(),
            languages: vec!["zh-CN".into()],
            include: vec!["docs/**/*.md".into()],
            exclude: vec![],
            data_dir: ".fani".into(),
            target_pattern: "docs/{lang}/{relpath}".into(),
            max_tasks: 40,
            repair_budget: 2,
            quality: QualityConfig::default(),
            documentation: DocumentationConfig::default(),
            publish: PublishConfig {
                enabled: false,
                branch: "i18n/{lang}".into(),
                push: false,
                remote: "origin".into(),
                source_ref: "HEAD".into(),
                github: GithubConfig::default(),
            },
        };
        let revision = resolve_source_revision(&repo).unwrap();
        fs::write(tmp.path().join("docs/a.md"), "new\n").unwrap();
        let documents = discover(&repo, &revision).unwrap();
        assert_eq!(documents[0].bytes, b"old\n");
    }
}
