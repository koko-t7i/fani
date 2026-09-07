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
    preflight(repo, source_revision, &repo.reserved_paths)
}

fn intersects(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn absolute_path(path: &Path) -> Result<std::path::PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component),
        }
        if normalized.exists() {
            normalized = normalized.canonicalize()?;
        }
    }
    Ok(normalized)
}

/// Validate every configured language before reading source contents or dispatching work.
pub fn preflight(
    repo: &RepoConfig,
    source_revision: &str,
    reserved: &[std::path::PathBuf],
) -> Result<Vec<SourceDocument>> {
    use crate::application::settings::SourceSet;
    use crate::domain::document::DocumentFormat;
    let legacy = vec![SourceSet {
        format: DocumentFormat::Markdown,
        include: if repo.include.is_empty() {
            vec!["**/*.md".into(), "*.md".into()]
        } else {
            repo.include.clone()
        },
        exclude: repo.exclude.clone(),
        strip_prefix: None,
        target_pattern: repo.target_pattern.clone(),
        message_syntax: None,
    }];
    let sets = repo.sources.as_ref().unwrap_or(&legacy);
    let rules = sets
        .iter()
        .map(|set| Ok((globset(&set.include, &[])?, globset(&set.exclude, &[])?)))
        .collect::<Result<Vec<_>>>()?;
    let listing = git(
        &repo.path,
        &["ls-tree", "-r", "--name-only", "-z", source_revision],
    )?;
    let repository = repo
        .path
        .canonicalize()
        .unwrap_or_else(|_| repo.path.clone());
    let mut documents = Vec::new();
    let mut counts = vec![0; sets.len()];
    for raw in listing
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
    {
        let path = String::from_utf8(raw.to_vec()).context("Git path is not UTF-8")?;
        let matches = rules
            .iter()
            .enumerate()
            .filter(|(_, (include, exclude))| include.is_match(&path) && !exclude.is_match(&path))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(anyhow!("source-set overlap: {path}"));
        }
        let Some(&index) = matches.first() else {
            continue;
        };
        let set = &sets[index];
        if set.format != DocumentFormat::Markdown {
            return Err(anyhow!(
                "source format is unavailable; only markdown is enabled"
            ));
        }
        if Path::new(&path).extension().and_then(|ext| ext.to_str()) != Some("md") {
            return Err(anyhow!(
                "unsupported source extension: {path}; migrate legacy include/exclude rules to explicit repo.sources with a supported format"
            ));
        }
        let relative = match &set.strip_prefix {
            Some(prefix) => Path::new(&path)
                .strip_prefix(prefix)
                .with_context(|| format!("source {path} is outside strip_prefix {prefix}"))?,
            None => Path::new(&path),
        };
        if relative.as_os_str().is_empty() {
            return Err(anyhow!(
                "strip_prefix removes the entire source path: {path}"
            ));
        }
        let mapped_relpath = relative
            .to_str()
            .context("source path is not UTF-8")?
            .to_owned();
        counts[index] += 1;
        let identity = hash(&[serde_json::to_string(set)?.as_bytes()]);
        let mapping_identity = hash(&[
            identity.as_bytes(),
            path.as_bytes(),
            mapped_relpath.as_bytes(),
            set.target_pattern.as_bytes(),
        ]);
        documents.push(SourceDocument {
            source_format: set.format,
            source_set_id: identity,
            mapping_identity,
            mapped_relpath,
            target_pattern: set.target_pattern.clone(),
            message_syntax: set.message_syntax,
            repository: repository.display().to_string(),
            source_revision: source_revision.to_owned(),
            path,
            bytes: Vec::new(),
            content_hash: String::new(),
        });
    }
    for (index, set) in sets.iter().enumerate() {
        if !set.target_pattern.contains("{relpath}") && counts[index] != 1 {
            return Err(anyhow!(
                "single-file mapping requires exactly one source file; found {} (missing source file or multiple matches)",
                counts[index]
            ));
        }
    }
    for document in &documents {
        let input = repository.join(&document.path);
        // A dangling directory alias can become reachable when targets are created.
        for ancestor in input.ancestors() {
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(anyhow!(
                        "input source path aliases another filesystem path: {}",
                        document.path
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut targets: Vec<std::path::PathBuf> = Vec::new();
    let mut reserved_paths = vec![
        repository.join(".git"),
        repository.join(&repo.data_dir),
        repository.join(".fani-report"),
    ];
    reserved_paths.push(std::env::current_dir()?.join(".fani-report"));
    for path in reserved {
        reserved_paths.push(absolute_path(path)?);
    }
    let reserved_paths = reserved_paths
        .iter()
        .map(|path| absolute_path(path))
        .collect::<Result<Vec<_>>>()?;
    for document in &documents {
        for language in &repo.languages {
            let target = document.target_path(language);
            if target.as_os_str().is_empty()
                || target.is_absolute()
                || target.components().any(|c| {
                    !matches!(
                        c,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
            {
                return Err(anyhow!("unsafe target path: {}", target.display()));
            }
            let absolute_target = absolute_path(&repository.join(&target))?;
            if !absolute_target.starts_with(&repository) {
                return Err(anyhow!(
                    "target path escapes repository: {}",
                    target.display()
                ));
            }
            if target.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case(".git"))
            }) || reserved_paths
                .iter()
                .any(|path| intersects(&absolute_target, path))
            {
                return Err(anyhow!(
                    "target overlaps reserved state/report/Git path: {}",
                    target.display()
                ));
            }
            if documents.iter().any(|source| {
                intersects(&target, Path::new(&source.path))
                    || intersects(&absolute_target, &repository.join(&source.path))
            }) {
                return Err(anyhow!(
                    "target overwrites an input source: {}",
                    target.display()
                ));
            }
            if rules.iter().any(|(include, exclude)| {
                (include.is_match(&target) && !exclude.is_match(&target))
                    || absolute_target
                        .strip_prefix(&repository)
                        .is_ok_and(|path| include.is_match(path) && !exclude.is_match(path))
            }) {
                return Err(anyhow!(
                    "target would be rediscovered as a source: {}; exclude generated targets",
                    target.display()
                ));
            }
            if targets
                .iter()
                .any(|other| intersects(&absolute_target, other))
            {
                return Err(anyhow!(
                    "target collision across source sets or languages: {}",
                    target.display()
                ));
            }
            targets.push(absolute_target);
        }
    }
    for document in &mut documents {
        let object = format!("{source_revision}:{}", document.path);
        document.bytes = git(&repo.path, &["show", &object])?;
        document.content_hash = hash(&[&document.bytes]);
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

    fn fixture(files: &[&str]) -> (tempfile::TempDir, RepoConfig, String) {
        let tmp = tempdir().unwrap();
        command(tmp.path(), &["init", "-q", "-b", "main"]);
        command(
            tmp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        command(tmp.path(), &["config", "user.name", "Test"]);
        for file in files {
            let path = tmp.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "Hello\n").unwrap();
        }
        command(tmp.path(), &["add", "."]);
        command(tmp.path(), &["commit", "-qm", "source"]);
        let repo: RepoConfig = toml::from_str(&format!(
            "path = {:?}\nlanguages = ['fr', 'zh-CN']\ninclude = ['docs/**']\nexclude = ['out/**']\ntarget_pattern = 'out/{{lang}}/{{relpath}}'",
            tmp.path().to_str().unwrap()
        )).unwrap();
        let revision = resolve_source_revision(&repo).unwrap();
        (tmp, repo, revision)
    }

    fn source(include: &str, target: &str) -> crate::application::settings::SourceSet {
        crate::application::settings::SourceSet {
            format: crate::domain::document::DocumentFormat::Markdown,
            include: vec![include.into()],
            exclude: Vec::new(),
            strip_prefix: None,
            target_pattern: target.into(),
            message_syntax: None,
        }
    }

    #[test]
    fn explicit_directory_and_filename_mappings_carry_stable_identity() {
        let (tmp, mut repo, revision) = fixture(&["docs/a.md", "README.md"]);
        let mut directory = source("docs/**", "out/{lang}/{relpath}");
        directory.strip_prefix = Some("docs/".into());
        repo.sources = Some(vec![directory, source("README.md", "readme/{lang}.md")]);
        let documents = discover(&repo, &revision).unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(
            documents[0].target_path("zh-CN"),
            Path::new("readme/zh-CN.md")
        );
        assert_eq!(documents[1].target_path("fr"), Path::new("out/fr/a.md"));
        assert_eq!(
            documents[1].source_format,
            crate::domain::document::DocumentFormat::Markdown
        );
        fs::write(tmp.path().join("docs/a.md"), "working tree changed").unwrap();
        assert_eq!(discover(&repo, &revision).unwrap(), documents);
        repo.sources.as_mut().unwrap()[0].target_pattern = "other/{lang}/{relpath}".into();
        let changed = discover(&repo, &revision).unwrap();
        assert_ne!(documents[1].mapping_identity, changed[1].mapping_identity);
        assert_eq!(documents[0].source_set_id, changed[0].source_set_id);
    }

    #[test]
    fn preflight_rejects_overlap_prefix_missing_source_and_extension_mismatch() {
        let (_tmp, mut repo, revision) = fixture(&["docs/a.md", "docs-extra/b.md", "README.txt"]);
        repo.sources = Some(vec![
            source("docs/**", "out/{lang}/{relpath}"),
            source("docs/a.md", "other/{lang}.md"),
        ]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("overlap")
        );
        let mut set = source("docs-extra/**", "out/{lang}/{relpath}");
        set.strip_prefix = Some("docs".into());
        repo.sources = Some(vec![set]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("outside strip_prefix")
        );
        repo.sources = Some(vec![source("missing.md", "out/{lang}.md")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("missing source file")
        );
        repo.sources = None;
        repo.include = vec!["README.txt".into()];
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("migrate legacy")
        );
        repo.sources = Some(vec![source("README.txt", "out/{lang}.md")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("unsupported source extension")
        );
    }

    #[test]
    fn preflight_checks_all_languages_and_source_sets_for_collisions() {
        let (_tmp, mut repo, revision) = fixture(&["docs/a.md", "docs/b.md"]);
        repo.languages = vec!["en".into(), "fr".into()];
        repo.sources = Some(vec![
            source("docs/a.md", "out/{lang}/fr.md"),
            source("docs/b.md", "out/en/{lang}.md"),
        ]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
        repo.sources = Some(vec![
            source("docs/a.md", "out/{lang}/a.md"),
            source("docs/b.md", "out/{lang}/a.md/child.md"),
        ]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
        repo.sources = Some(vec![source("docs/**", "out/{lang}/same/{relpath}")]);
        repo.sources.as_mut().unwrap()[0].strip_prefix = Some("docs".into());
        assert_eq!(discover(&repo, &revision).unwrap().len(), 2);
    }

    #[test]
    fn preflight_rejects_input_overwrites_rediscovery_and_reserved_paths() {
        let (tmp, mut repo, revision) = fixture(&["docs/a.md"]);
        repo.languages = vec!["a".into()];
        repo.sources = Some(vec![source("docs/a.md", "docs/{lang}.md")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("input source")
        );
        repo.sources = Some(vec![source("**/*.md", "out/{lang}/{relpath}")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("rediscovered")
        );
        for root in [
            ".git",
            "nested/.git",
            ".fani",
            ".fani-report",
            "custom-reports",
        ] {
            repo.sources = Some(vec![source("docs/a.md", &format!("{root}/{{lang}}.md"))]);
            let error = preflight(
                &repo,
                &revision,
                &[tmp.path().join("not-created/../custom-reports")],
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("reserved"), "{root}: {error}");
        }
    }

    #[test]
    fn preflight_rejects_filesystem_aliases_to_state_sources_or_outside_repository() {
        use std::os::unix::fs::symlink;
        let (tmp, mut repo, revision) = fixture(&["docs/a.md"]);
        fs::create_dir(tmp.path().join(".fani")).unwrap();
        symlink(".fani", tmp.path().join("state-alias")).unwrap();
        repo.sources = Some(vec![source("docs/a.md", "state-alias/{lang}.md")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        symlink("docs", tmp.path().join("source-alias")).unwrap();
        repo.languages = vec!["a".into()];
        repo.sources = Some(vec![source("docs/a.md", "source-alias/{lang}.md")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("input source")
        );
        let outside = tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("outside")).unwrap();
        repo.sources = Some(vec![source("docs/a.md", "outside/{lang}.md")]);
        assert!(
            discover(&repo, &revision)
                .unwrap_err()
                .to_string()
                .contains("escapes repository")
        );
    }

    #[test]
    fn preflight_rejects_reverse_input_alias_even_when_target_does_not_exist() {
        use std::os::unix::fs::symlink;
        let (tmp, mut repo, revision) = fixture(&["docs/fr.md"]);
        repo.languages = vec!["fr".into()];
        repo.sources = Some(vec![source("docs/fr.md", "out/{lang}.md")]);
        fs::remove_dir_all(tmp.path().join("docs")).unwrap();
        symlink("out", tmp.path().join("docs")).unwrap();
        let error = discover(&repo, &revision).unwrap_err().to_string();
        assert!(error.contains("input source path aliases"), "{error}");
        fs::create_dir(tmp.path().join("out")).unwrap();
        let error = discover(&repo, &revision).unwrap_err().to_string();
        assert!(error.contains("input source path aliases"), "{error}");
        assert!(!tmp.path().join("out/fr.md").exists());
        assert_eq!(
            git(tmp.path(), &["show", &format!("{revision}:docs/fr.md")]).unwrap(),
            b"Hello\n"
        );
    }

    #[test]
    fn legacy_empty_include_keeps_markdown_default_without_sniffing() {
        let (_tmp, mut repo, revision) = fixture(&["docs/a.md", "other.mdx", "messages.json"]);
        repo.include.clear();
        assert_eq!(discover(&repo, &revision).unwrap().len(), 1);
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
            reserved_paths: Vec::new(),
            path: tmp.path().into(),
            languages: vec!["zh-CN".into()],
            sources: None,
            include: vec!["docs/**/*.md".into()],
            exclude: vec!["docs/zh-CN/**".into()],
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
