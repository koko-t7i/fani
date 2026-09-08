use crate::application::ports::{Materialization, MaterializationResult, Materializer};
use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub struct FilesystemMaterializer;

pub fn content_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(anyhow!(
            "materialization path escapes repository: {}",
            relative.display()
        ));
    }
    let root = root
        .canonicalize()
        .context("repository root does not exist")?;
    let mut candidate = root.clone();
    for component in relative.components() {
        if let Component::Normal(segment) = component {
            candidate.push(segment);
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(anyhow!(
                        "materialization path crosses a symlink: {}",
                        relative.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(candidate)
}

fn write_atomic(path: &Path, desired: &[u8]) -> Result<()> {
    let parent = path.parent().context("materialized target has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(desired)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn restore(root: &Path, operation: &Materialization) -> Result<MaterializationResult> {
    let path = safe_path(root, &operation.path)?;
    let desired_hash = content_hash(&operation.desired);
    if fs::read(&path).ok().as_deref() == Some(operation.desired.as_slice()) {
        return Ok(MaterializationResult::AlreadyCurrent { hash: desired_hash });
    }
    write_atomic(&path, &operation.desired)?;
    Ok(MaterializationResult::Written { hash: desired_hash })
}

pub fn apply(root: &Path, operation: &Materialization) -> Result<MaterializationResult> {
    let path = safe_path(root, &operation.path)?;
    let existing = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let desired_hash = content_hash(&operation.desired);
    if existing.as_deref() == Some(operation.desired.as_slice()) {
        return Ok(MaterializationResult::AlreadyCurrent { hash: desired_hash });
    }
    if let (Some(expected), Some(existing)) = (&operation.expected_hash, &existing) {
        let actual_hash = content_hash(existing);
        if &actual_hash != expected {
            return Ok(MaterializationResult::HumanEdit { actual_hash });
        }
    } else if operation.expected_hash.is_some() && existing.is_none() {
        return Ok(MaterializationResult::HumanEdit {
            actual_hash: "missing".into(),
        });
    } else if operation.expected_hash.is_none() && existing.is_some() {
        return Ok(MaterializationResult::HumanEdit {
            actual_hash: content_hash(existing.as_ref().expect("existing bytes")),
        });
    }
    write_atomic(&path, &operation.desired)?;
    Ok(MaterializationResult::Written { hash: desired_hash })
}

impl Materializer for FilesystemMaterializer {
    fn read(&self, root: &Path, relative: &Path) -> Result<Option<Vec<u8>>> {
        let path = safe_path(root, relative)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn apply(&self, root: &Path, operation: &Materialization) -> Result<MaterializationResult> {
        apply(root, operation)
    }

    fn restore(&self, root: &Path, operation: &Materialization) -> Result<MaterializationResult> {
        restore(root, operation)
    }
}

impl crate::application::ports::TargetReader for FilesystemMaterializer {
    fn read(&self, root: &Path, relative: &Path) -> Result<Option<Vec<u8>>> {
        <Self as Materializer>::read(self, root, relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn repeat_is_idempotent_and_human_edit_is_not_overwritten() {
        let tmp = tempdir().unwrap();
        let operation = Materialization {
            path: "docs/zh/a.md".into(),
            expected_hash: None,
            desired: b"translated\n".to_vec(),
        };
        assert!(matches!(
            apply(tmp.path(), &operation).unwrap(),
            MaterializationResult::Written { .. }
        ));
        let expected = content_hash(b"translated\n");
        let repeat = Materialization {
            expected_hash: Some(expected.clone()),
            ..operation.clone()
        };
        assert!(matches!(
            apply(tmp.path(), &repeat).unwrap(),
            MaterializationResult::AlreadyCurrent { .. }
        ));
        fs::write(tmp.path().join("docs/zh/a.md"), "human\n").unwrap();
        assert!(matches!(
            apply(tmp.path(), &repeat).unwrap(),
            MaterializationResult::HumanEdit { .. }
        ));
        assert_eq!(
            fs::read(tmp.path().join("docs/zh/a.md")).unwrap(),
            b"human\n"
        );
    }
}
