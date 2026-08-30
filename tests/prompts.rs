use fani::domain::{
    model::{AgentStage, AgentTask},
    prompts,
};
use sha2::{Digest, Sha256};

fn task(stage: AgentStage) -> AgentTask {
    AgentTask {
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
            "e5542d0c289e9d5f968a48d09c561697078455da0141a7f214986bf489f468db",
        ),
        (
            "revise",
            prompts::REVISE,
            "2d88ac3badd067088a6dadaac8bf31e29ec9b70846b662099a4740a43a934414",
        ),
        (
            "repair",
            prompts::REPAIR,
            "9915da0ff97327796a0a7b68cbee1704edf511a52a161a392f0e9da0677c6082",
        ),
        (
            "revision",
            prompts::REVISION,
            "31285f6338bc7645981b38f27229e2c0bd947829fa880d84c1595b88c612f7dd",
        ),
        (
            "proofread",
            prompts::PROOFREAD,
            "28d53c65af3fde48957f1c47967809ce5804f85375ce78b484f9b9342be05f0b",
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
