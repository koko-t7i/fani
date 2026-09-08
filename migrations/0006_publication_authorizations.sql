CREATE TABLE publication_manifests_new (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES runs(id) ON DELETE SET NULL,
    locale TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    candidate_commit TEXT NOT NULL,
    policy_fingerprint TEXT NOT NULL CHECK (length(policy_fingerprint) = 64),
    state TEXT NOT NULL CHECK (state IN ('commit_created','push_pending','pr_open','merged','superseded')),
    created_at INTEGER NOT NULL,
    merged_at INTEGER,
    authorization_key TEXT NOT NULL,
    UNIQUE(repository_id, locale, authorization_key)
) STRICT;

INSERT INTO publication_manifests_new
SELECT m.*, COALESCE(
    (SELECT o.dedupe_key FROM publication_outbox o
     WHERE o.repository_id=m.repository_id AND o.locale=m.locale
       AND json_extract(o.payload_json, '$.run_id')=m.run_id
       AND json_extract(o.payload_json, '$.commit')=m.candidate_commit
     ORDER BY o.id LIMIT 1),
    'run:' || COALESCE(m.run_id, '') || ':' || m.candidate_commit)
FROM publication_manifests m;
DROP TABLE publication_manifests;
ALTER TABLE publication_manifests_new RENAME TO publication_manifests;
CREATE INDEX publication_manifests_commit
    ON publication_manifests(repository_id, locale, candidate_commit, state, id);

UPDATE state_schema SET version=6 WHERE singleton=1;
