CREATE TABLE unit_versions (
    id INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES units(id) ON DELETE CASCADE,
    source_revision TEXT NOT NULL,
    source_text TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    context_json TEXT NOT NULL CHECK (json_valid(context_json)),
    created_at INTEGER NOT NULL,
    UNIQUE(unit_id, source_revision, source_hash)
) STRICT;
CREATE INDEX unit_versions_source ON unit_versions(unit_id, source_revision);

INSERT INTO unit_versions(unit_id, source_revision, source_text, source_hash, context_json, created_at)
SELECT u.id, COALESCE(d.source_revision, ''), u.source_text, u.source_hash, u.context_json, u.created_at
FROM units u
JOIN documents d ON d.id=u.document_id;

CREATE TABLE translation_versions (
    id INTEGER PRIMARY KEY,
    unit_version_id INTEGER NOT NULL REFERENCES unit_versions(id) ON DELETE RESTRICT,
    locale TEXT NOT NULL,
    target_text TEXT NOT NULL,
    target_hash TEXT NOT NULL,
    freshness TEXT NOT NULL CHECK (freshness IN ('exact','source_changed','structurally_changed','orphaned')),
    provenance TEXT NOT NULL CHECK (provenance IN ('human','imported','trusted_tm','candidate_tm','ai','repaired_ai')),
    validation_state TEXT NOT NULL CHECK (validation_state IN ('pending','passed','failed','quarantined')),
    review_state TEXT NOT NULL CHECK (review_state IN ('unreviewed','needs_review','approved','rejected')),
    publication_state TEXT NOT NULL CHECK (publication_state IN ('candidate','commit_created','push_pending','pr_open','merged','superseded')),
    policy_fingerprint TEXT NOT NULL CHECK (length(policy_fingerprint) = 64),
    source_attempt_id INTEGER REFERENCES attempts(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    superseded_at INTEGER
) STRICT;
CREATE INDEX translation_versions_unit_locale ON translation_versions(unit_version_id, locale, superseded_at);

CREATE TABLE translation_memory_entries (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    unit_id INTEGER REFERENCES units(id) ON DELETE SET NULL,
    translation_version_id INTEGER REFERENCES translation_versions(id) ON DELETE SET NULL,
    locale TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    source_revision TEXT NOT NULL DEFAULT '',
    context_key TEXT NOT NULL DEFAULT '',
    target_text TEXT NOT NULL,
    tier TEXT NOT NULL CHECK (tier IN ('trusted','candidate','history')),
    provenance TEXT NOT NULL,
    policy_fingerprint TEXT NOT NULL CHECK (length(policy_fingerprint) = 64),
    created_at INTEGER NOT NULL,
    superseded_at INTEGER
) STRICT;
CREATE UNIQUE INDEX one_active_tm_entry
ON translation_memory_entries(repository_id, locale, source_hash, context_key, tier)
WHERE superseded_at IS NULL;
CREATE INDEX translation_memory_lookup
ON translation_memory_entries(repository_id, locale, source_hash, context_key, tier, superseded_at);

INSERT INTO translation_memory_entries(
    repository_id,unit_id,locale,source_hash,context_key,target_text,tier,provenance,
    policy_fingerprint,created_at,superseded_at)
SELECT repository_id,unit_id,locale,source_hash,context_key,target_text,'trusted',provenance,
       lower(hex(zeroblob(32))),trusted_at,superseded_at
FROM trusted_translation_memory;

ALTER TABLE runs ADD COLUMN policy_fingerprint TEXT NOT NULL DEFAULT '';
ALTER TABLE work_items ADD COLUMN policy_fingerprint TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN provider TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN model TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN adapter TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN provider_fingerprint TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN prompt_version TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN prompt_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE attempts ADD COLUMN policy_fingerprint TEXT NOT NULL DEFAULT '';

ALTER TABLE canonical_files ADD COLUMN freshness TEXT NOT NULL DEFAULT 'exact'
    CHECK (freshness IN ('exact','source_changed','structurally_changed','orphaned'));
ALTER TABLE canonical_files ADD COLUMN provenance TEXT NOT NULL DEFAULT 'ai'
    CHECK (provenance IN ('human','imported','trusted_tm','candidate_tm','ai','repaired_ai'));
ALTER TABLE canonical_files ADD COLUMN validation_state TEXT NOT NULL DEFAULT 'pending'
    CHECK (validation_state IN ('pending','passed','failed','quarantined'));
ALTER TABLE canonical_files ADD COLUMN review_state TEXT NOT NULL DEFAULT 'unreviewed'
    CHECK (review_state IN ('unreviewed','needs_review','approved','rejected'));
ALTER TABLE canonical_files ADD COLUMN publication_state TEXT NOT NULL DEFAULT 'candidate'
    CHECK (publication_state IN ('candidate','commit_created','push_pending','pr_open','merged','superseded'));
ALTER TABLE canonical_files ADD COLUMN trust_tier TEXT NOT NULL DEFAULT 'candidate'
    CHECK (trust_tier IN ('trusted','candidate','history'));
ALTER TABLE canonical_files ADD COLUMN policy_fingerprint TEXT NOT NULL DEFAULT '';

CREATE TABLE canonical_content_versions (
    id INTEGER PRIMARY KEY,
    canonical_file_id INTEGER NOT NULL REFERENCES canonical_files(id) ON DELETE CASCADE,
    source_revision TEXT NOT NULL,
    content BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    publication_state TEXT NOT NULL DEFAULT 'candidate'
        CHECK (publication_state IN ('candidate','commit_created','push_pending','pr_open','merged','superseded')),
    created_at INTEGER NOT NULL,
    UNIQUE(canonical_file_id, content_hash),
    UNIQUE(id, canonical_file_id, content_hash)
) STRICT;

INSERT INTO canonical_content_versions(
    canonical_file_id,source_revision,content,content_hash,publication_state,created_at)
SELECT id,source_revision,content,content_hash,publication_state,updated_at
FROM canonical_files;

ALTER TABLE canonical_files ADD COLUMN current_content_version_id INTEGER
    REFERENCES canonical_content_versions(id) ON DELETE RESTRICT;

UPDATE canonical_files
SET current_content_version_id=(
    SELECT ccv.id FROM canonical_content_versions ccv
    WHERE ccv.canonical_file_id=canonical_files.id
      AND ccv.content_hash=canonical_files.content_hash
);

CREATE TABLE canonical_file_translations (
    canonical_content_version_id INTEGER NOT NULL REFERENCES canonical_content_versions(id) ON DELETE CASCADE,
    translation_version_id INTEGER NOT NULL REFERENCES translation_versions(id) ON DELETE RESTRICT,
    PRIMARY KEY(canonical_content_version_id, translation_version_id)
) WITHOUT ROWID, STRICT;

CREATE TABLE publication_manifests (
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
    UNIQUE(repository_id, locale, candidate_commit)
) STRICT;

CREATE TABLE publication_manifest_files (
    manifest_id INTEGER NOT NULL REFERENCES publication_manifests(id) ON DELETE CASCADE,
    canonical_content_version_id INTEGER NOT NULL,
    canonical_file_id INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY(manifest_id, canonical_content_version_id),
    FOREIGN KEY(canonical_content_version_id, canonical_file_id, content_hash)
        REFERENCES canonical_content_versions(id, canonical_file_id, content_hash) ON DELETE RESTRICT
) WITHOUT ROWID, STRICT;

UPDATE state_schema SET version=2 WHERE singleton=1;
