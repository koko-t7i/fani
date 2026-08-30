CREATE TABLE state_schema (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    installed_at INTEGER NOT NULL
) STRICT;

CREATE TABLE repositories (
    id INTEGER PRIMARY KEY,
    repository_key TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL UNIQUE,
    default_branch TEXT,
    remote_url TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    source_revision TEXT,
    content_hash TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    deleted_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(repository_id, path)
) STRICT;
CREATE INDEX documents_repository ON documents(repository_id, deleted_at);

CREATE TABLE units (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    unit_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    source_text TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    context_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(context_json)),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(document_id, unit_key)
) STRICT;
CREATE INDEX units_source_hash ON units(source_hash);

CREATE TABLE trusted_translation_memory (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    unit_id INTEGER REFERENCES units(id) ON DELETE SET NULL,
    locale TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    context_key TEXT NOT NULL DEFAULT '',
    target_text TEXT NOT NULL,
    provenance TEXT NOT NULL,
    trusted_at INTEGER NOT NULL,
    superseded_at INTEGER,
    UNIQUE(repository_id, locale, source_hash, context_key)
) STRICT;
CREATE INDEX trusted_tm_lookup ON trusted_translation_memory(repository_id, locale, source_hash, superseded_at);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    repository_id INTEGER REFERENCES repositories(id) ON DELETE RESTRICT,
    invocation_key TEXT UNIQUE,
    config_path TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'running' CHECK (status IN ('running','ok','partial','needs_human','error','cancelled')),
    started_at INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    finished_at INTEGER,
    exit_code INTEGER,
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    CHECK ((finished_at IS NULL AND exit_code IS NULL) OR finished_at IS NOT NULL)
) STRICT;
CREATE INDEX runs_repository_status ON runs(repository_id, status, started_at);

CREATE TABLE work_items (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    unit_id INTEGER NOT NULL REFERENCES units(id) ON DELETE RESTRICT,
    locale TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','running','succeeded','failed','cancelled')),
    priority INTEGER NOT NULL DEFAULT 0,
    input_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(input_json)),
    result_json TEXT CHECK (result_json IS NULL OR json_valid(result_json)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(run_id, unit_id, locale, kind)
) STRICT;
CREATE INDEX work_items_claim ON work_items(run_id, status, priority DESC, id);

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    work_item_id INTEGER NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    dedupe_key TEXT NOT NULL,
    attempt_no INTEGER NOT NULL CHECK (attempt_no > 0),
    agent TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('started','succeeded','failed','timed_out','cancelled')),
    request_json TEXT NOT NULL CHECK (json_valid(request_json)),
    response_json TEXT CHECK (response_json IS NULL OR json_valid(response_json)),
    error TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    UNIQUE(work_item_id, dedupe_key),
    UNIQUE(work_item_id, attempt_no)
) STRICT;
CREATE INDEX attempts_work_item ON attempts(work_item_id, id);

CREATE TABLE findings (
    id INTEGER PRIMARY KEY,
    work_item_id INTEGER NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    attempt_id INTEGER REFERENCES attempts(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('info','warning','error','blocking')),
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    details_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(details_json)),
    resolved_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE(work_item_id, fingerprint)
) STRICT;

CREATE TABLE canonical_candidates (
    id INTEGER PRIMARY KEY,
    unit_id INTEGER NOT NULL REFERENCES units(id) ON DELETE CASCADE,
    locale TEXT NOT NULL,
    candidate_key TEXT NOT NULL,
    target_text TEXT NOT NULL,
    source_attempt_id INTEGER REFERENCES attempts(id) ON DELETE SET NULL,
    score REAL,
    selected INTEGER NOT NULL DEFAULT 0 CHECK (selected IN (0, 1)),
    created_at INTEGER NOT NULL,
    UNIQUE(unit_id, locale, candidate_key)
) STRICT;
CREATE UNIQUE INDEX one_selected_candidate ON canonical_candidates(unit_id, locale) WHERE selected = 1;

CREATE TABLE canonical_files (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    locale TEXT NOT NULL,
    path TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    content BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    materialized_hash TEXT,
    state TEXT NOT NULL DEFAULT 'candidate' CHECK (state IN ('candidate','materialized','human_edit','adopted','published','merged')),
    updated_at INTEGER NOT NULL,
    UNIQUE(repository_id, locale, path)
) STRICT;
CREATE INDEX canonical_files_state ON canonical_files(repository_id, locale, state);

CREATE TABLE materialization_outbox (
    id INTEGER PRIMARY KEY,
    work_item_id INTEGER NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    dedupe_key TEXT NOT NULL UNIQUE,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','processing','done')),
    available_at INTEGER NOT NULL,
    owner TEXT,
    lease_expires_at INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    CHECK ((state = 'processing') = (owner IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK ((state = 'done') = (completed_at IS NOT NULL))
) STRICT;
CREATE INDEX materialization_ready ON materialization_outbox(state, available_at, id);

CREATE TABLE publication_outbox (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES runs(id) ON DELETE SET NULL,
    locale TEXT NOT NULL,
    dedupe_key TEXT NOT NULL UNIQUE,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','processing','done')),
    available_at INTEGER NOT NULL,
    owner TEXT,
    lease_expires_at INTEGER,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    CHECK ((state = 'processing') = (owner IS NOT NULL AND lease_expires_at IS NOT NULL)),
    CHECK ((state = 'done') = (completed_at IS NOT NULL))
) STRICT;
CREATE INDEX publication_ready ON publication_outbox(state, available_at, id);

CREATE TABLE pull_requests (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    number INTEGER,
    branch TEXT NOT NULL,
    url TEXT,
    state TEXT NOT NULL CHECK (state IN ('draft','open','merged','closed')),
    head_revision TEXT,
    opened_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    closed_at INTEGER,
    UNIQUE(repository_id, provider, external_id)
) STRICT;

CREATE TABLE pr_events (
    id INTEGER PRIMARY KEY,
    pull_request_id INTEGER NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
    event_key TEXT NOT NULL,
    from_state TEXT,
    to_state TEXT NOT NULL CHECK (to_state IN ('draft','open','merged','closed')),
    payload_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload_json)),
    occurred_at INTEGER NOT NULL,
    UNIQUE(pull_request_id, event_key)
) STRICT;

CREATE TABLE leases (
    resource_type TEXT NOT NULL,
    resource_key TEXT NOT NULL,
    owner TEXT NOT NULL,
    fencing_token INTEGER NOT NULL CHECK (fencing_token > 0),
    acquired_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY(resource_type, resource_key),
    CHECK (expires_at > acquired_at)
) WITHOUT ROWID, STRICT;
CREATE INDEX leases_expiry ON leases(expires_at);

INSERT INTO state_schema(singleton, generation, version, installed_at)
VALUES (1, 'native-authoritative', 1, unixepoch('subsec') * 1000);
