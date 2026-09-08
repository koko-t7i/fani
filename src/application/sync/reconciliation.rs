use super::*;

pub fn adopt_human_edit(
    repo: &RepoConfig,
    database: &dyn StateStore,
    materializer: &dyn Materializer,
    git: &dyn GitPublisher,
    documentation: &dyn DocumentationChecker,
    language: &str,
) -> Result<usize> {
    let repository_key = repo
        .path
        .canonicalize()
        .unwrap_or_else(|_| repo.path.clone());
    let repository_id = database.upsert_repository(
        &hash(&[repository_key.to_string_lossy().as_bytes()]),
        &repo.path,
        Some(&repo.publish.github.base),
        None,
    )?;
    let source_revision = git.resolve_source_revision(repo)?;
    let documents = git.discover(repo, &source_revision)?;
    let mut adoption_files = Vec::new();
    for document in &documents {
        let target = document
            .target_path(language)
            .to_string_lossy()
            .into_owned();
        if database
            .canonical_file(repository_id, language, &target)?
            .is_none()
        {
            continue;
        }
        let bytes = materializer
            .read(&repo.path, Path::new(&target))?
            .with_context(|| format!("cannot read human target {target}"))?;
        let source = document.parse()?;
        let translated = document.parse_bytes(&bytes)?;
        verify_document(&source, &translated.source)
            .with_context(|| format!("human target {target} failed validation"))?;
        if source.units.len() != translated.units.len()
            || source
                .units
                .iter()
                .zip(&translated.units)
                .any(|(source, target)| {
                    crate::domain::document::translated_unit_text(source, target).is_none()
                })
        {
            bail!("human target {target} failed validation");
        }
        adoption_files.push(PublicationFile {
            path: target,
            content: bytes,
        });
    }
    if adoption_files.is_empty() {
        return Ok(0);
    }
    let manifest = adoption_files
        .iter()
        .map(|file| json!({"path": file.path, "content_hash": content_hash(&file.content)}))
        .collect::<Vec<_>>();
    let manifest_hash = content_hash(&serde_json::to_vec(&manifest)?);
    let run_id = database.begin_run(
        repository_id,
        &format!("adopt:{repository_id}:{language}:{source_revision}:{manifest_hash}"),
        Path::new("adopt"),
        "{}",
        &prompts::policy_fingerprint(),
    )?;
    let anchor = documents
        .iter()
        .find(|document| document.target_path(language).to_string_lossy() == adoption_files[0].path)
        .ok_or_else(|| anyhow!("adoption candidate has no document identity"))?;
    let anchor_id = database
        .document_id(repository_id, &anchor.path)?
        .ok_or_else(|| anyhow!("adoption document is missing"))?;
    let check_work = if repo.documentation.commands.is_empty() {
        None
    } else {
        Some(database.enqueue_document_work_item(&run_id, anchor_id, language, "project_check", 0, &json!({"source_revision":source_revision,"manifest":manifest,"manifest_hash":manifest_hash}).to_string())?)
    };
    let checked = documentation
        .check(repo, &source_revision, &adoption_files)?
        .failures
        .is_empty();
    if let Some(work) = check_work {
        database.finish_document_work(
            work,
            checked,
            &json!({"manifest_hash":manifest_hash}).to_string(),
        )?;
    }
    if !checked {
        database.finish_run(&run_id, "needs_human")?;
        bail!("human targets failed configured documentation checks; no translations were trusted");
    }
    let mut adopted = 0;
    for document in documents {
        let target = document
            .target_path(language)
            .to_string_lossy()
            .into_owned();
        let Some(canonical) = database.canonical_file(repository_id, language, &target)? else {
            continue;
        };
        let bytes = adoption_files
            .iter()
            .find(|file| file.path == target)
            .ok_or_else(|| anyhow!("human target {target} was not checked"))?
            .content
            .clone();
        let target_text = std::str::from_utf8(&bytes).context("human target is not UTF-8")?;
        let source_document = document.parse()?;
        let target_document = document.parse_bytes(target_text.as_bytes())?;
        verify_document(&source_document, target_text)
            .with_context(|| format!("human target {target} failed validation"))?;
        let source_units = &source_document.units;
        let target_units = &target_document.units;
        if source_units.len() != target_units.len() {
            bail!("human target {target} does not preserve the source document unit structure");
        }
        let translations = source_units
            .iter()
            .zip(target_units)
            .map(|(source, translated)| {
                crate::domain::document::translated_unit_text(source, translated)
                    .ok_or_else(|| anyhow!("human target {target} failed validation"))
            })
            .collect::<Result<Vec<_>>>()?;
        let stable_ids = source_units
            .iter()
            .enumerate()
            .map(|(ordinal, unit)| stable_unit_id(&document.path, unit, ordinal))
            .collect::<Vec<_>>();
        let stable_hints = stable_unit_hints(
            database,
            repository_id,
            &document.path,
            &document.content_hash,
            &stable_ids,
        )?;
        let history = match database.document_id(repository_id, &document.path)? {
            Some(document_id) => database.unit_history(document_id, language)?,
            None => Vec::new(),
        };
        let matched = match_units_with_stable_ids(
            &previous_units(&history, &[], &document.path),
            source_units,
            &stable_hints,
        );
        if matched
            .iter()
            .any(|matched| matched.kind == MatchKind::Ambiguous)
        {
            bail!("human target {target} cannot be mapped to stable source units");
        }
        let document_id = database.upsert_document(
            repository_id,
            &document.path,
            Some(&source_revision),
            &document.content_hash,
            &serde_json::to_string(
                &json!({"format": source_document.format, "contract": source_document.contract}),
            )?,
        )?;
        let identity = document_identity(&document, language, &source_document);
        let assembly_work = database.enqueue_document_work_item(
            &run_id,
            document_id,
            language,
            "assembly",
            0,
            &serde_json::to_string(&identity)?,
        )?;
        database.finish_document_work(
            assembly_work,
            true,
            &json!({"content_hash": content_hash(&bytes)}).to_string(),
        )?;
        for (ordinal, ((source_unit, translated), matched)) in source_units
            .iter()
            .zip(&translations)
            .zip(matched)
            .enumerate()
        {
            let stable_id = matched
                .stable_id
                .unwrap_or_else(|| stable_ids[ordinal].clone());
            let source_hash = hash(&[source_unit.source.as_bytes()]);
            let context = source_unit.memory_context_key(&document.path);
            let unit_id = database.upsert_unit(
                document_id,
                &stable_id,
                ordinal as i64,
                &source_unit.source,
                &source_hash,
                &unit_metadata(source_unit, &document.path),
            )?;
            database.trust_translation(TrustTranslationInput {
                repository_id,
                unit_id: Some(unit_id),
                locale: language,
                source_hash: &source_hash,
                context_key: &context,
                target_text: translated,
                provenance: "human_adopted",
                policy_fingerprint: &prompts::policy_fingerprint(),
            })?;
        }
        let adopted_hash = content_hash(&bytes);
        database.persist_canonical_document(
            CanonicalFileInput {
                repository_id,
                locale: language,
                path: &target,
                source_revision: &source_revision,
                content: &bytes,
                content_hash: &adopted_hash,
                materialized_hash: Some(&adopted_hash),
                freshness: Freshness::Exact,
                provenance: if source_units.is_empty() {
                    TranslationProvenance::Imported
                } else {
                    TranslationProvenance::Human
                },
                validation: ValidationState::Passed,
                review: ReviewState::Approved,
                publication: PublicationState::Candidate,
                trust_tier: MemoryTier::Trusted,
                policy_fingerprint: &prompts::policy_fingerprint(),
            },
            &[],
            &serde_json::to_string(&identity)?,
        )?;
        database.transition_canonical_file(
            canonical.id,
            CanonicalTransition::Adopted,
            Some(&adopted_hash),
        )?;
        let materialization_work = database.enqueue_document_work_item(
            &run_id,
            document_id,
            language,
            "materialization",
            0,
            &serde_json::to_string(&identity)?,
        )?;
        database.finish_document_work(
            materialization_work,
            true,
            &json!({"status":"adopted", "content_hash":adopted_hash}).to_string(),
        )?;
        adopted += 1;
    }
    database.finish_run(&run_id, "ok")?;
    Ok(adopted)
}

pub fn discard_human_edit(
    repo: &RepoConfig,
    database: &dyn StateStore,
    materializer: &dyn Materializer,
    git: &dyn GitPublisher,
    language: &str,
) -> Result<usize> {
    let repository_key = repo
        .path
        .canonicalize()
        .unwrap_or_else(|_| repo.path.clone());
    let repository_id = database.upsert_repository(
        &hash(&[repository_key.to_string_lossy().as_bytes()]),
        &repo.path,
        Some(&repo.publish.github.base),
        None,
    )?;
    let source_revision = git.resolve_source_revision(repo)?;
    let documents = git.discover(repo, &source_revision)?;
    let mut discarded = 0;
    for document in documents {
        let target = document
            .target_path(language)
            .to_string_lossy()
            .into_owned();
        let Some(canonical) = database.canonical_file(repository_id, language, &target)? else {
            continue;
        };
        let operation = Materialization {
            path: target.clone().into(),
            expected_hash: None,
            desired: canonical.content.clone(),
        };
        let result = materializer.restore(&repo.path, &operation)?;
        let hash = match result {
            MaterializationResult::Written { hash }
            | MaterializationResult::AlreadyCurrent { hash } => hash,
            MaterializationResult::HumanEdit { .. } => {
                return Err(anyhow!("cannot discard {target}"));
            }
        };
        database.transition_canonical_file(
            canonical.id,
            CanonicalTransition::Materialized,
            Some(&hash),
        )?;
        discarded += 1;
    }
    Ok(discarded)
}
