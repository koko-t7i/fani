//! Compatibility entrypoints for database fixtures; application services use business capabilities.
use super::*;

impl Database {
    pub fn promote_verified_publication(
        &self,
        repository_id: i64,
        locale: &str,
        candidate_commit: &str,
        provenance: &str,
        verified_zero_unit_contents: &[i64],
    ) -> Result<usize> {
        self.promote_publication(
            repository_id,
            locale,
            candidate_commit,
            provenance,
            verified_zero_unit_contents,
        )
    }
}
