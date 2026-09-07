CREATE TABLE canonical_content_versions_new (
    id INTEGER PRIMARY KEY,
    canonical_file_id INTEGER NOT NULL REFERENCES canonical_files(id) ON DELETE CASCADE,
    source_revision TEXT NOT NULL,
    content BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    publication_state TEXT NOT NULL DEFAULT 'candidate'
        CHECK (publication_state IN ('candidate','commit_created','push_pending','pr_open','merged','superseded')),
    created_at INTEGER NOT NULL,
    UNIQUE(canonical_file_id, source_revision, content_hash),
    UNIQUE(id, canonical_file_id, content_hash)
) STRICT;

INSERT INTO canonical_content_versions_new
SELECT * FROM canonical_content_versions;
DROP TABLE canonical_content_versions;
ALTER TABLE canonical_content_versions_new RENAME TO canonical_content_versions;

CREATE TABLE canonical_document_intents (
    id INTEGER PRIMARY KEY,
    canonical_content_version_id INTEGER NOT NULL REFERENCES canonical_content_versions(id) ON DELETE RESTRICT,
    identity_json TEXT NOT NULL CHECK (json_valid(identity_json)),
    created_at INTEGER NOT NULL,
    UNIQUE(canonical_content_version_id, identity_json)
) STRICT;

UPDATE state_schema SET version=4 WHERE singleton=1;
