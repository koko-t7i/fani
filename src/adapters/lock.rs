use crate::adapters::db::{Database, Lease};
use crate::adapters::process::current_process_identity;
use anyhow::{Result, anyhow};
use chrono::Utc;
use std::path::Path;

#[derive(Debug)]
pub struct RepoLock {
    db: Database,
    lease: Option<Lease>,
}

impl RepoLock {
    pub fn acquire(db: Database, repo: &Path) -> Result<Self> {
        Self::acquire_with_stale(db, repo, 24 * 60 * 60)
    }

    pub fn acquire_with_stale(db: Database, repo: &Path, stale_after_s: i64) -> Result<Self> {
        let key = repo
            .canonicalize()
            .unwrap_or_else(|_| repo.to_owned())
            .display()
            .to_string();
        let (pid, started_at) = current_process_identity()?;
        let owner = format!("{pid}:{started_at}:{}", Utc::now().timestamp_millis());
        let lease = db
            .acquire_lease(
                "repository",
                &key,
                &owner,
                Utc::now().timestamp_millis(),
                stale_after_s.saturating_mul(1000),
            )?
            .ok_or_else(|| anyhow!("another run holds the repository lease for {key}"))?;
        Ok(Self {
            db,
            lease: Some(lease),
        })
    }

    pub fn release(&mut self) -> Result<()> {
        if let Some(lease) = self.lease.take() {
            if !self.db.release_lease(&lease)? {
                return Err(anyhow!("repository lease ownership changed before release"));
            }
        }
        Ok(())
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sqlite_lease_excludes_a_second_owner_and_releases() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let mut first = RepoLock::acquire(db.clone(), tmp.path()).unwrap();
        assert!(RepoLock::acquire(db.clone(), tmp.path()).is_err());
        first.release().unwrap();
        RepoLock::acquire(db, tmp.path()).unwrap();
    }

    #[test]
    fn expired_lease_is_replaced() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        db.acquire_lease("repository", "key", "dead", 1, 1)
            .unwrap()
            .unwrap();
        assert!(
            db.acquire_lease("repository", "key", "new", 3, 100)
                .unwrap()
                .is_some()
        );
    }
}
