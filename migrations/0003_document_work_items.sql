CREATE TABLE work_items_new (
    id INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    unit_id INTEGER REFERENCES units(id) ON DELETE RESTRICT,
    document_id INTEGER REFERENCES documents(id) ON DELETE RESTRICT,
    locale TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','running','succeeded','failed','cancelled')),
    priority INTEGER NOT NULL DEFAULT 0,
    input_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(input_json)),
    result_json TEXT CHECK (result_json IS NULL OR json_valid(result_json)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    policy_fingerprint TEXT NOT NULL DEFAULT '',
    CHECK ((unit_id IS NOT NULL) != (document_id IS NOT NULL))
) STRICT;

INSERT INTO work_items_new(
    id,run_id,unit_id,locale,kind,status,priority,input_json,result_json,
    created_at,updated_at,policy_fingerprint)
SELECT id,run_id,unit_id,locale,kind,status,priority,input_json,result_json,
       created_at,updated_at,policy_fingerprint
FROM work_items;

DROP TABLE work_items;
ALTER TABLE work_items_new RENAME TO work_items;
CREATE INDEX work_items_claim ON work_items(run_id, status, priority DESC, id);
CREATE UNIQUE INDEX work_items_unit_scope
    ON work_items(run_id, unit_id, locale, kind) WHERE unit_id IS NOT NULL;
CREATE UNIQUE INDEX work_items_document_scope
    ON work_items(run_id, document_id, locale, kind) WHERE document_id IS NOT NULL;

UPDATE state_schema SET version=3 WHERE singleton=1;
