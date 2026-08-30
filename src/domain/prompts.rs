use crate::domain::model::{AgentStage, AgentTask};
use sha2::{Digest, Sha256};

pub const PROMPT_VERSION: &str = "fani-native-markdown-prompts-v2";
pub const VERIFIER_VERSION: &str = "fani-markdown-verifier-v1";

pub const TRANSLATE: &str = include_str!("../../prompts/translate.md");
pub const REVISE: &str = include_str!("../../prompts/revise.md");
pub const REPAIR: &str = include_str!("../../prompts/repair.md");
pub const REVISION: &str = include_str!("../../prompts/revision.md");
pub const PROOFREAD: &str = include_str!("../../prompts/proofread.md");

pub const TRANSLATION_RULES: &str = TRANSLATE;
pub const REPAIR_RULES: &str = REPAIR;

pub fn resource_name(task: &AgentTask) -> &'static str {
    match task.stage {
        AgentStage::Translate
            if task.previous_source.is_some() && task.previous_translation.is_some() =>
        {
            "revise"
        }
        AgentStage::Translate => "translate",
        AgentStage::Repair => "repair",
        AgentStage::Revision => "revision",
        AgentStage::Proofread => "proofread",
    }
}

pub fn resource(task: &AgentTask) -> &'static str {
    match resource_name(task) {
        "translate" => TRANSLATE,
        "revise" => REVISE,
        "repair" => REPAIR,
        "revision" => REVISION,
        "proofread" => PROOFREAD,
        _ => unreachable!("all prompt resources are matched"),
    }
}

pub fn prompt_hash(stage: &AgentStage) -> String {
    let rules = match stage {
        AgentStage::Translate => TRANSLATE,
        AgentStage::Repair => REPAIR,
        AgentStage::Revision => REVISION,
        AgentStage::Proofread => PROOFREAD,
    };
    format!("{:x}", Sha256::digest(rules.as_bytes()))
}

pub fn task_prompt_hash(task: &AgentTask) -> String {
    format!("{:x}", Sha256::digest(resource(task).as_bytes()))
}

pub fn policy_fingerprint() -> String {
    let mut digest = Sha256::new();
    for resource in [
        PROMPT_VERSION,
        VERIFIER_VERSION,
        TRANSLATE,
        REVISE,
        REPAIR,
        REVISION,
        PROOFREAD,
    ] {
        digest.update((resource.len() as u64).to_be_bytes());
        digest.update(resource.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub fn render(task: &AgentTask) -> String {
    let mut context = String::new();
    if let Some(previous_source) = &task.previous_source {
        context.push_str("\n\n--- PREVIOUS SOURCE ---\n");
        context.push_str(previous_source);
        context.push_str("\n--- END PREVIOUS SOURCE ---");
    }
    if let Some(previous_translation) = &task.previous_translation {
        context.push_str("\n\n--- PREVIOUS TRANSLATION ---\n");
        context.push_str(previous_translation);
        context.push_str("\n--- END PREVIOUS TRANSLATION ---");
    }
    if !task.findings.is_empty() {
        context.push_str("\n\nFindings:\n");
        for finding in &task.findings {
            context.push_str("- ");
            context.push_str(&finding.code);
            context.push_str(": ");
            context.push_str(&finding.message);
            context.push('\n');
        }
    }

    format!(
        "{PROMPT_VERSION}\nResource: {}\n\n{}\nSource language: {}\nTarget language: {}\nTask ID: {}\nProtected tokens: {}{context}\n\n--- SOURCE ---\n{}\n--- END SOURCE ---",
        resource_name(task),
        resource(task),
        task.source_language,
        task.target_language,
        task.id,
        task.protected_tokens.join(", "),
        task.source
    )
}

pub fn translation_prompt(target_language: &str, unit_id: &str, protected_source: &str) -> String {
    format!(
        "{PROMPT_VERSION}\nResource: translate\n\n{TRANSLATE}\nTarget language: {target_language}\nUnit ID: {unit_id}\n\n--- SOURCE ---\n{protected_source}\n--- END SOURCE ---"
    )
}

pub fn repair_prompt(
    target_language: &str,
    unit_id: &str,
    protected_source: &str,
    candidate: &str,
    findings: &[String],
) -> String {
    let findings = findings
        .iter()
        .map(|finding| format!("- {finding}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{PROMPT_VERSION}\nResource: repair\n\n{REPAIR}\nTarget language: {target_language}\nUnit ID: {unit_id}\n\nFindings:\n{findings}\n\n--- SOURCE ---\n{protected_source}\n--- END SOURCE ---\n\n--- CANDIDATE ---\n{candidate}\n--- END CANDIDATE ---"
    )
}
