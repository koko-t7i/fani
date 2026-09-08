CREATE TABLE canonical_document_intent_selection (
    canonical_content_version_id INTEGER PRIMARY KEY REFERENCES canonical_content_versions(id) ON DELETE RESTRICT,
    intent_id INTEGER NOT NULL REFERENCES canonical_document_intents(id) ON DELETE RESTRICT
) STRICT;

INSERT INTO canonical_document_intent_selection
SELECT canonical_content_version_id, MAX(id)
FROM canonical_document_intents GROUP BY canonical_content_version_id;

DROP INDEX work_items_document_scope;
CREATE UNIQUE INDEX work_items_document_scope
    ON work_items(run_id, document_id, locale, kind, COALESCE(json_extract(input_json, '$.effect_key'), ''))
    WHERE document_id IS NOT NULL;

UPDATE state_schema SET version=5 WHERE singleton=1;
