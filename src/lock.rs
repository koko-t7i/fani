use crate::db::Database;
use anyhow::{Context, Result};
use chrono::Utc;
use nix::fcntl::{RenameFlags, renameat2};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use rusqlite::{OptionalExtension, params};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("another run holds the repository lock for {0}")]
pub struct LockBusy(pub String);

#[derive(Debug)]
pub struct RepoLock {
    db: Database,
    repo: String,
    legacy_path: PathBuf,
    acquired: bool,
}

impl RepoLock {
    pub fn acquire(db: Database, repo: &Path) -> Result<Self> {
        Self::acquire_with_stale(db, repo, 24 * 3600)
    }

    pub fn acquire_with_stale(db: Database, repo: &Path, stale_after_s: i64) -> Result<Self> {
        let repo_key = repo
            .canonicalize()
            .unwrap_or_else(|_| repo.to_path_buf())
            .display()
            .to_string();
        let legacy_path = db
            .path()
            .parent()
            .context("fani.db has no state directory")?
            .join("fani.lock");
        acquire_legacy_lock(&legacy_path, stale_after_s)?;

        let sqlite_result = (|| -> Result<()> {
            let mut conn = db.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(i32, i64)> = tx
                .query_row(
                    "SELECT pid, started_at FROM repo_locks WHERE repo=?1",
                    [&repo_key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((pid, started_at)) = owner {
                let age = Utc::now().timestamp() - started_at;
                let alive = age <= stale_after_s && process_alive(pid);
                if alive {
                    return Err(LockBusy(repo_key.clone()).into());
                }
                tx.execute("DELETE FROM repo_locks WHERE repo=?1", [&repo_key])?;
            }
            tx.execute(
                "INSERT INTO repo_locks(repo,pid,started_at) VALUES (?1,?2,?3)",
                params![repo_key, std::process::id() as i64, Utc::now().timestamp()],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if let Err(err) = sqlite_result {
            release_legacy_lock(&legacy_path);
            return Err(err);
        }

        Ok(Self {
            db,
            repo: repo_key,
            legacy_path,
            acquired: true,
        })
    }

    pub fn release(&mut self) -> Result<()> {
        if self.acquired {
            let sqlite_result = (|| -> Result<()> {
                self.db.connect()?.execute(
                    "DELETE FROM repo_locks WHERE repo=?1 AND pid=?2",
                    params![self.repo, std::process::id() as i64],
                )?;
                Ok(())
            })();
            release_legacy_lock(&self.legacy_path);
            self.acquired = false;
            sqlite_result?;
        }
        Ok(())
    }
}

fn acquire_legacy_lock(path: &Path, stale_after_s: i64) -> Result<()> {
    let parent = path
        .parent()
        .context("legacy lock has no state directory")?;
    fs::create_dir_all(parent)?;
    let payload = serde_json::json!({
        "pid": std::process::id(),
        "started": Utc::now().timestamp_millis() as f64 / 1000.0,
    })
    .to_string();
    let mut prepared = tempfile::NamedTempFile::new_in(parent)?;
    prepared.write_all(payload.as_bytes())?;
    prepared.as_file().sync_all()?;
    let parent_dir = File::open(parent)?;
    let prepared_name = prepared
        .path()
        .file_name()
        .context("prepared legacy lock has no filename")?;
    let lock_name = path.file_name().context("legacy lock has no filename")?;

    for _ in 0..3 {
        match fs::hard_link(prepared.path(), path) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let snapshot = legacy_lock_snapshot(path, stale_after_s);
                match snapshot.state {
                    LegacyLockState::Live | LegacyLockState::Initializing => {
                        return Err(LockBusy(path.display().to_string()).into());
                    }
                    LegacyLockState::Stale => match renameat2(
                        &parent_dir,
                        prepared_name,
                        &parent_dir,
                        lock_name,
                        RenameFlags::RENAME_EXCHANGE,
                    ) {
                        Ok(()) => {
                            let replaced = fs::metadata(prepared.path())?;
                            if replaced.dev() == snapshot.dev && replaced.ino() == snapshot.ino {
                                return Ok(());
                            }
                            renameat2(
                                &parent_dir,
                                prepared_name,
                                &parent_dir,
                                lock_name,
                                RenameFlags::RENAME_EXCHANGE,
                            )?;
                            return Err(LockBusy(path.display().to_string()).into());
                        }
                        Err(nix::errno::Errno::ENOENT) => continue,
                        Err(err) => return Err(err.into()),
                    },
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(LockBusy(path.display().to_string()).into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LegacyLockState {
    Live,
    Stale,
    Initializing,
}

#[derive(Clone, Copy, Debug)]
struct LegacyLockSnapshot {
    state: LegacyLockState,
    dev: u64,
    ino: u64,
}

fn legacy_lock_snapshot(path: &Path, stale_after_s: i64) -> LegacyLockSnapshot {
    let metadata = fs::metadata(path);
    let (dev, ino) = metadata
        .as_ref()
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .unwrap_or_default();
    let content = fs::read_to_string(path);
    let value = content
        .as_deref()
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
    let owner = value.as_ref().and_then(|value| {
        Some((
            value.get("pid")?.as_i64()? as i32,
            value.get("started")?.as_f64()?,
        ))
    });
    let state = if let Some((pid, started)) = owner {
        let age = Utc::now().timestamp() as f64 - started;
        if age <= stale_after_s as f64 && process_alive(pid) {
            LegacyLockState::Live
        } else {
            LegacyLockState::Stale
        }
    } else {
        let recently_created = metadata
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age < std::time::Duration::from_secs(2));
        if recently_created {
            LegacyLockState::Initializing
        } else {
            LegacyLockState::Stale
        }
    };
    LegacyLockSnapshot { state, dev, ino }
}

fn release_legacy_lock(path: &Path) {
    let owned = fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|value| value.get("pid").and_then(serde_json::Value::as_u64))
        == Some(std::process::id() as u64);
    if owned {
        let _ = fs::remove_file(path);
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn process_alive(pid: i32) -> bool {
    if pid == std::process::id() as i32 {
        return true;
    }
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_live_owner_is_rejected_and_release_allows_next() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let mut first = RepoLock::acquire(db.clone(), tmp.path()).unwrap();
        assert!(
            RepoLock::acquire(db.clone(), tmp.path())
                .unwrap_err()
                .to_string()
                .contains("another run")
        );
        first.release().unwrap();
        RepoLock::acquire(db, tmp.path()).unwrap();
    }

    #[test]
    fn stale_owner_is_replaced() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let repo = tmp.path().canonicalize().unwrap().display().to_string();
        db.connect()
            .unwrap()
            .execute(
                "INSERT INTO repo_locks(repo,pid,started_at) VALUES (?1,?2,0)",
                params![repo, 4_194_303],
            )
            .unwrap();
        RepoLock::acquire_with_stale(db, tmp.path(), 60).unwrap();
    }

    #[test]
    fn live_python_compatible_lock_is_rejected() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        fs::write(
            tmp.path().join("fani.lock"),
            serde_json::json!({
                "pid": std::process::id(),
                "started": Utc::now().timestamp(),
            })
            .to_string(),
        )
        .unwrap();

        let error = RepoLock::acquire(db, tmp.path()).unwrap_err();
        assert!(error.to_string().contains("another run"));
    }

    #[test]
    fn stale_python_lock_is_atomically_replaced() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let path = tmp.path().join("fani.lock");
        fs::write(
            &path,
            serde_json::json!({"pid": 4_194_303, "started": 0}).to_string(),
        )
        .unwrap();

        let mut lock = RepoLock::acquire_with_stale(db, tmp.path(), 60).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["pid"], std::process::id());
        lock.release().unwrap();
    }

    #[test]
    fn fresh_incomplete_python_lock_is_not_removed() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let path = tmp.path().join("fani.lock");
        fs::write(&path, "").unwrap();

        let error = RepoLock::acquire(db, tmp.path()).unwrap_err();
        assert!(error.to_string().contains("another run"));
        assert!(path.exists());
        assert_eq!(fs::read(&path).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn writes_and_releases_python_compatible_lock() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let path = tmp.path().join("fani.lock");
        let mut lock = RepoLock::acquire(db, tmp.path()).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(value["pid"], std::process::id());
        assert!(value["started"].as_f64().unwrap() > 0.0);
        lock.release().unwrap();
        assert!(!path.exists());
    }
}
