use super::*;

impl Database {
    pub fn acquire_lease(
        &self,
        resource_type: &str,
        resource_key: &str,
        owner: &str,
        now: i64,
        ttl_ms: i64,
    ) -> Result<Option<Lease>> {
        if ttl_ms <= 0 {
            bail!("lease TTL must be positive");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<Lease> = tx
            .query_row(
                "SELECT resource_type,resource_key,owner,fencing_token,acquired_at,expires_at FROM leases WHERE resource_type=?1 AND resource_key=?2",
                params![resource_type, resource_key],
                lease_from_row,
            )
            .optional()?;
        let lease = match current {
            Some(current)
                if current.owner != owner
                    && current.expires_at > now
                    && !(resource_type == "repository"
                        && dead_repository_owner(&current.owner)) =>
            {
                tx.commit()?;
                return Ok(None);
            }
            Some(current) => Lease {
                resource_type: resource_type.into(),
                resource_key: resource_key.into(),
                owner: owner.into(),
                fencing_token: if current.owner == owner {
                    current.fencing_token
                } else {
                    current.fencing_token + 1
                },
                acquired_at: now,
                expires_at: now + ttl_ms,
            },
            None => Lease {
                resource_type: resource_type.into(),
                resource_key: resource_key.into(),
                owner: owner.into(),
                fencing_token: 1,
                acquired_at: now,
                expires_at: now + ttl_ms,
            },
        };
        tx.execute(
            r#"INSERT INTO leases(resource_type,resource_key,owner,fencing_token,acquired_at,expires_at)
               VALUES (?1,?2,?3,?4,?5,?6)
               ON CONFLICT(resource_type,resource_key) DO UPDATE SET
                 owner=excluded.owner,fencing_token=excluded.fencing_token,
                 acquired_at=excluded.acquired_at,expires_at=excluded.expires_at"#,
            params![lease.resource_type, lease.resource_key, lease.owner, lease.fencing_token, lease.acquired_at, lease.expires_at],
        )?;
        tx.commit()?;
        Ok(Some(lease))
    }

    pub fn renew_lease(&self, lease: &Lease, now: i64, ttl_ms: i64) -> Result<Option<Lease>> {
        if ttl_ms <= 0 {
            bail!("lease TTL must be positive");
        }
        let expires_at = now + ttl_ms;
        let changed = self.connect()?.execute(
            "UPDATE leases SET expires_at=?6 WHERE resource_type=?1 AND resource_key=?2 AND owner=?3 AND fencing_token=?4 AND expires_at>?5",
            params![lease.resource_type, lease.resource_key, lease.owner, lease.fencing_token, now, expires_at],
        )?;
        Ok((changed == 1).then(|| Lease {
            expires_at,
            ..lease.clone()
        }))
    }

    pub fn release_lease(&self, lease: &Lease) -> Result<bool> {
        Ok(self.connect()?.execute(
            "DELETE FROM leases WHERE resource_type=?1 AND resource_key=?2 AND owner=?3 AND fencing_token=?4",
            params![lease.resource_type, lease.resource_key, lease.owner, lease.fencing_token],
        )? == 1)
    }
}
