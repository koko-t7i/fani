use crate::adapters::db::{Database, Lease};
use crate::adapters::process::current_process_identity;
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use nix::fcntl::{Flock, FlockArg};
use std::fs::File;
use std::path::Path;

#[derive(Debug)]
pub struct RepositoryGuard {
    key: String,
    process_lock: Flock<File>,
}

impl RepositoryGuard {
    /// Acquire before opening/migrating the database to cover all repository writes.
    pub fn acquire(repo: &Path) -> Result<Self> {
        let repository = repo.canonicalize()?;
        let key = repository.display().to_string();
        // The kernel lock, not the expiring lease, owns repository exclusion.
        // Lock the directory inode so aliases and alternate database paths share
        // the same lock and no removable sidecar can split ownership. File::open
        // uses O_CLOEXEC, so spawned Git/agent executables cannot retain it.
        let directory = File::open(&repository)
            .with_context(|| format!("cannot open repository directory {key}"))?;
        let process_lock =
            Flock::lock(directory, FlockArg::LockExclusiveNonblock).map_err(|(_, error)| {
                anyhow!("cannot acquire repository process lock for {key}: {error}")
            })?;
        Ok(Self { key, process_lock })
    }
}

#[derive(Debug)]
pub struct RepoLock {
    db: Database,
    lease: Option<Lease>,
    process_lock: Option<Flock<File>>,
}

impl RepoLock {
    #[cfg(test)]
    pub fn acquire(db: Database, repo: &Path) -> Result<Self> {
        Self::acquire_with_stale(db, repo, 24 * 60 * 60)
    }

    #[cfg(test)]
    pub fn acquire_with_stale(db: Database, repo: &Path, stale_after_s: i64) -> Result<Self> {
        Self::acquire_guarded_with_stale(db, RepositoryGuard::acquire(repo)?, stale_after_s)
    }

    pub fn acquire_guarded(db: Database, guard: RepositoryGuard) -> Result<Self> {
        Self::acquire_guarded_with_stale(db, guard, 24 * 60 * 60)
    }

    fn acquire_guarded_with_stale(
        db: Database,
        guard: RepositoryGuard,
        stale_after_s: i64,
    ) -> Result<Self> {
        let RepositoryGuard { key, process_lock } = guard;
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
            process_lock: Some(process_lock),
        })
    }

    pub fn release(&mut self) -> Result<()> {
        if let Some(lease) = self.lease.as_ref() {
            if !self.db.release_lease(lease)? {
                return Err(anyhow!("repository lease ownership changed before release"));
            }
        }
        self.lease = None;
        // Drop the kernel lock only after the recovery metadata is released.
        self.process_lock = None;
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
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // Invoked in a separate test process by the parent regression test.
    #[test]
    fn repository_lock_child() {
        let Some(root) = std::env::var_os("FANI_LOCK_TEST_ROOT") else {
            return;
        };
        let root = Path::new(&root);
        let db = Database::open(root.join("fani.db")).unwrap();
        let _lock = RepoLock::acquire_with_stale(db, root, 1).unwrap();
        std::fs::write(root.join("ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn paused_process_keeps_expired_lease_exclusive_and_exit_recovers() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "adapters::lock::tests::repository_lock_child"])
                .env("FANI_LOCK_TEST_ROOT", tmp.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !tmp.path().join("ready").exists() {
            assert!(child.0.try_wait().unwrap().is_none(), "lock holder exited");
            assert!(
                Instant::now() < deadline,
                "lock holder did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        kill(Pid::from_raw(child.0.id() as i32), Signal::SIGSTOP).unwrap();
        // Expire the lease deterministically, without a 24-hour or TTL wait.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE leases SET acquired_at=0,expires_at=1 WHERE resource_type='repository'",
                [],
            )
            .unwrap();
        assert!(RepoLock::acquire(db.clone(), tmp.path()).is_err());
        // Exclusion must also work with a different SQLite database.
        let other_db = Database::open(tmp.path().join("other.db")).unwrap();
        assert!(RepoLock::acquire(other_db, tmp.path()).is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        RepoLock::acquire(db, tmp.path()).unwrap();
    }

    #[test]
    fn directory_lock_is_close_on_exec_and_drop_releases_it() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let first = RepoLock::acquire(db.clone(), tmp.path()).unwrap();
        let flags = fcntl(&**first.process_lock.as_ref().unwrap(), FcntlArg::F_GETFD).unwrap();
        assert!(FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC));
        drop(first);
        RepoLock::acquire(db, tmp.path()).unwrap();
    }

    #[test]
    fn guard_excludes_before_database_initialization() {
        let tmp = tempdir().unwrap();
        let guard = RepositoryGuard::acquire(tmp.path()).unwrap();
        assert!(RepositoryGuard::acquire(tmp.path()).is_err());
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let lock = RepoLock::acquire_guarded(db.clone(), guard).unwrap();
        assert!(RepositoryGuard::acquire(tmp.path()).is_err());
        drop(lock);
        RepoLock::acquire(db, tmp.path()).unwrap();
    }

    #[test]
    fn failed_metadata_release_keeps_lock_until_drop() {
        let tmp = tempdir().unwrap();
        let db = Database::open(tmp.path().join("fani.db")).unwrap();
        let mut lock = RepoLock::acquire(db.clone(), tmp.path()).unwrap();
        db.connect()
            .unwrap()
            .execute("DELETE FROM leases", [])
            .unwrap();
        assert!(lock.release().is_err());
        assert!(RepositoryGuard::acquire(tmp.path()).is_err());
        drop(lock);
        RepoLock::acquire(db, tmp.path()).unwrap();
    }

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
