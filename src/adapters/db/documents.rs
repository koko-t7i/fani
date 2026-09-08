use super::*;

impl Database {
    pub fn upsert_repository(
        &self,
        repository_key: &str,
        root_path: &Path,
        default_branch: Option<&str>,
        remote_url: Option<&str>,
    ) -> Result<i64> {
        let now = now_ms();
        let root = root_path.display().to_string();
        let conn = self.connect()?;
        conn.execute(
            r#"INSERT INTO repositories(repository_key,root_path,default_branch,remote_url,created_at,updated_at)
               VALUES (?1,?2,?3,?4,?5,?5)
               ON CONFLICT(repository_key) DO UPDATE SET
                 root_path=excluded.root_path,
                 default_branch=excluded.default_branch,
                 remote_url=excluded.remote_url,
                 updated_at=excluded.updated_at"#,
            params![repository_key, root, default_branch, remote_url, now],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM repositories WHERE repository_key=?1",
            [repository_key],
            |row| row.get(0),
        )?)
    }

    pub fn upsert_document(
        &self,
        repository_id: i64,
        path: &str,
        source_revision: Option<&str>,
        content_hash: &str,
        metadata_json: &str,
    ) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = preparation::upsert_document_in_transaction(
            &tx,
            repository_id,
            path,
            source_revision,
            content_hash,
            metadata_json,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn document_id(&self, repository_id: i64, path: &str) -> Result<Option<i64>> {
        Ok(self
            .connect()?
            .query_row(
                "SELECT id FROM documents WHERE repository_id=?1 AND path=?2 AND deleted_at IS NULL",
                params![repository_id, path],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn upsert_unit(
        &self,
        document_id: i64,
        unit_key: &str,
        ordinal: i64,
        source_text: &str,
        source_hash: &str,
        context_json: &str,
    ) -> Result<i64> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = preparation::upsert_unit_in_transaction(
            &tx,
            document_id,
            unit_key,
            ordinal,
            source_text,
            source_hash,
            context_json,
        )?;
        tx.commit()?;
        Ok(result)
    }

    pub fn unit_history(&self, document_id: i64, locale: &str) -> Result<Vec<UnitHistory>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            r#"SELECT u.id,u.unit_key,u.ordinal,COALESCE(uv.source_text,u.source_text),
                      COALESCE(uv.source_hash,u.source_hash),COALESCE(uv.context_json,u.context_json),
                      CASE WHEN uv.id IS NOT NULL THEN t.target_text END,
                      CASE WHEN uv.id IS NULL THEN 0 ELSE 1 END
               FROM units u
               LEFT JOIN translation_memory_entries t ON t.id=(
                 SELECT m.id FROM translation_memory_entries m WHERE m.unit_id=u.id AND m.locale=?2
                   AND m.tier='trusted' AND m.superseded_at IS NULL ORDER BY m.id DESC LIMIT 1)
               LEFT JOIN translation_versions tv ON tv.id=t.translation_version_id
               LEFT JOIN unit_versions uv ON uv.id=COALESCE(tv.unit_version_id,
                 (SELECT v.id FROM unit_versions v WHERE v.unit_id=t.unit_id AND v.source_hash=t.source_hash
                    AND (t.source_revision='' OR v.source_revision=t.source_revision) ORDER BY v.id LIMIT 1))
                 AND uv.source_hash=t.source_hash
               WHERE u.document_id=?1 AND u.active=1 ORDER BY u.ordinal,u.id"#,
        )?;
        let rows = statement.query_map(params![document_id, locale], |row| {
            Ok(UnitHistory {
                id: row.get(0)?,
                unit_key: row.get(1)?,
                ordinal: row.get::<_, i64>(2)? as usize,
                source_text: row.get(3)?,
                source_hash: row.get(4)?,
                context_json: row.get(5)?,
                translation: row.get(6)?,
                trusted: row.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unchanged_document_unit_keys(
        &self,
        repository_id: i64,
        path: &str,
        content_hash: &str,
    ) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut statement = conn.prepare(
            r#"SELECT u.unit_key
               FROM units u
               JOIN documents d ON d.id=u.document_id
               WHERE d.repository_id=?1 AND d.path=?2 AND d.content_hash=?3
                 AND d.deleted_at IS NULL AND u.active=1
                 AND EXISTS (
                     SELECT 1 FROM unit_versions uv
                     WHERE uv.unit_id=u.id
                       AND uv.source_revision=COALESCE(d.source_revision,'')
                       AND uv.source_hash=u.source_hash
                       AND uv.source_text=u.source_text
                 )
               ORDER BY u.ordinal,u.id"#,
        )?;
        let rows =
            statement.query_map(params![repository_id, path, content_hash], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

impl Database {
    pub fn repository_id(&self, repository_key: &str) -> Result<Option<i64>> {
        PlanningStore::repository_id(self, repository_key)
    }
}
