use fani::domain::{
    model::{AgentStage, AgentTask},
    prompts,
};
use sha2::{Digest, Sha256};

fn task(stage: AgentStage) -> AgentTask {
    AgentTask {
        source_format: fani::domain::document::DocumentFormat::Markdown,
        unit_context: fani::domain::document::UnitContext::markdown(),
        context_key: "fixture-context".into(),
        message_syntax: None,
        token_permissions: fani::domain::model::TokenPermissions {
            contract: "fani-markdown-tokens-v1".into(),
            reorderable_tokens: Vec::new(),
        },
        id: "unit-1".into(),
        stage,
        source_language: "en".into(),
        target_language: "zh-CN".into(),
        source: "Hello @@FANI_CODE_0@@".into(),
        previous_source: None,
        previous_translation: None,
        findings: Vec::new(),
        protected_tokens: vec!["@@FANI_CODE_0@@".into()],
    }
}

#[test]
fn embedded_prompt_resources_match_reviewed_golden_hashes() {
    let resources = [
        (
            "translate",
            prompts::TRANSLATE,
            "96579857f1841c49e5ffaf3149ed6f50ebf8123c2e7a6a1d0478dbeefdd37136",
        ),
        (
            "revise",
            prompts::REVISE,
            "7d99f3bff649505d1795afd1ccec92cbca54f2567cf67a0e3a53193dc3115c7a",
        ),
        (
            "repair",
            prompts::REPAIR,
            "65ee2c277bac714a35dff3e5dfba030aab5ab8b9d7f7f29f92803640d3334c0f",
        ),
        (
            "revision",
            prompts::REVISION,
            "242b03e51fefd8030a7c2707511a877fddaf86912f55ab5a98ed00c431903436",
        ),
        (
            "proofread",
            prompts::PROOFREAD,
            "27ce9a8c1133c800aa46e6a585b4159b8ff9c7074dd1c2f4ccbd8cb9ef3e655d",
        ),
    ];
    for (name, resource, expected) in resources {
        assert_eq!(
            format!("{:x}", Sha256::digest(resource.as_bytes())),
            expected,
            "{name}"
        );
        assert!(
            resource.ends_with('\n'),
            "{name} must retain a stable final newline"
        );
    }
}

#[test]
fn each_agent_behavior_selects_its_versioned_resource() {
    for (stage, name, resource) in [
        (AgentStage::Translate, "translate", prompts::TRANSLATE),
        (AgentStage::Repair, "repair", prompts::REPAIR),
        (AgentStage::Revision, "revision", prompts::REVISION),
        (AgentStage::Proofread, "proofread", prompts::PROOFREAD),
    ] {
        let task = task(stage);
        assert_eq!(prompts::resource_name(&task), name);
        assert_eq!(prompts::resource(&task), resource);
        let rendered = prompts::render(&task);
        assert!(rendered.contains(&format!("Resource: {name}")));
        assert!(rendered.contains(resource));
        assert_eq!(
            prompts::task_prompt_hash(&task),
            format!("{:x}", Sha256::digest(resource.as_bytes()))
        );
    }

    let mut revise = task(AgentStage::Translate);
    revise.previous_source = Some("Old source".into());
    revise.previous_translation = Some("旧译文".into());
    assert_eq!(prompts::resource_name(&revise), "revise");
    assert_eq!(prompts::resource(&revise), prompts::REVISE);
    let rendered = prompts::render(&revise);
    assert!(rendered.contains("--- PREVIOUS SOURCE ---\nOld source"));
    assert!(rendered.contains("--- PREVIOUS TRANSLATION ---\n旧译文"));
}

#[test]
fn request_v2_exposes_stable_context_and_safe_token_permissions_for_every_stage() {
    use fani::application::ports::{
        AGENT_REQUEST_SCHEMA, AgentPolicy, AgentPrompt, AgentRequestEnvelope,
    };
    use fani::domain::document::{DocumentFormat, parse_document, validate_unit};
    use fani::domain::model::TokenPermissions;
    let parsed = parse_document(DocumentFormat::Markdown, b"Use `one` before `two`.\n").unwrap();
    let unit = &parsed.units[0];
    let permissions = TokenPermissions::for_unit(unit);
    assert_eq!(permissions.reorderable_tokens.len(), 2);
    let swapped = unit
        .protected_source
        .replace(&permissions.reorderable_tokens[0], "TEMP")
        .replace(
            &permissions.reorderable_tokens[1],
            &permissions.reorderable_tokens[0],
        )
        .replace("TEMP", &permissions.reorderable_tokens[1]);
    validate_unit(unit, &swapped).unwrap();
    for stage in [
        AgentStage::Translate,
        AgentStage::Repair,
        AgentStage::Revision,
        AgentStage::Proofread,
    ] {
        let mut task = task(stage);
        task.unit_context = unit.context.clone();
        task.context_key = unit.memory_context_key("docs/a.md");
        task.token_permissions = permissions.clone();
        task.source = unit.protected_source.clone();
        task.protected_tokens = permissions.reorderable_tokens.clone();
        let rendered = prompts::render(&task);
        for field in [
            "Source format: \"markdown\"",
            "Unit context:",
            "Context key:",
            "Message syntax: null",
            "Token permissions:",
        ] {
            assert!(rendered.contains(field), "{rendered}");
        }
        let envelope = AgentRequestEnvelope {
            schema: AGENT_REQUEST_SCHEMA.into(),
            prompt: AgentPrompt {
                version: prompts::PROMPT_VERSION.into(),
                resource: prompts::resource_name(&task).into(),
                hash: prompts::task_prompt_hash(&task),
                content: rendered,
            },
            policy: AgentPolicy {
                fingerprint: prompts::policy_fingerprint(),
            },
            task,
        };
        let mut value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["schema"], "fani.agent.request.v2");
        assert_eq!(value["task"]["source_format"], "markdown");
        assert_eq!(
            serde_json::from_value::<AgentRequestEnvelope>(value.clone()).unwrap(),
            envelope
        );
        value["task"]
            .as_object_mut()
            .unwrap()
            .remove("token_permissions");
        assert!(serde_json::from_value::<AgentRequestEnvelope>(value).is_err());
    }
}

#[test]
fn policy_fingerprint_covers_every_prompt_and_verifier_version() {
    assert_eq!(prompts::policy_fingerprint().len(), 64);
    let baseline = prompts::policy_fingerprint();
    let mut digest = Sha256::new();
    for resource in [
        prompts::PROMPT_VERSION,
        prompts::VERIFIER_VERSION,
        prompts::TRANSLATE,
        prompts::REVISE,
        prompts::REPAIR,
        prompts::REVISION,
        prompts::PROOFREAD,
    ] {
        digest.update((resource.len() as u64).to_be_bytes());
        digest.update(resource.as_bytes());
    }
    assert_eq!(baseline, format!("{:x}", digest.finalize()));
}
