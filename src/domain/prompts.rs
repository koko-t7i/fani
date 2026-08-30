use crate::domain::model::{AgentStage, AgentTask};

pub const TRANSLATION_RULES: &str = r#"Translate only the Markdown unit between SOURCE markers.
Return only the translated Markdown unit, with no fence, explanation, or metadata.
Preserve Markdown structure. Every @@FANI_*@@ token is immutable: copy each token exactly once and do not invent tokens.
Do not translate code, HTML, link destinations, or placeholders represented by those tokens."#;

pub const REPAIR_RULES: &str = r#"Repair the candidate with the smallest possible edit.
Return only the repaired Markdown unit, with no fence, explanation, or metadata.
Preserve Markdown structure. Every @@FANI_*@@ token is immutable: copy each token exactly once and do not invent tokens."#;

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

    let action = match &task.stage {
        AgentStage::Translate => "Translate the source into the target language.",
        AgentStage::Repair => {
            "Repair the candidate according to the findings with the smallest possible edit."
        }
        AgentStage::Revision => "Revise the translation only where the source changed.",
        AgentStage::Proofread => "Proofread the translation without changing protected content.",
    };
    format!(
        "fani-native-markdown-prompt-v1\n\n{TRANSLATION_RULES}\n\n{action}\nSource language: {}\nTarget language: {}\nTask ID: {}\nProtected tokens: {}{context}\n\n--- SOURCE ---\n{}\n--- END SOURCE ---",
        task.source_language,
        task.target_language,
        task.id,
        task.protected_tokens.join(", "),
        task.source
    )
}

pub fn translation_prompt(target_language: &str, unit_id: &str, protected_source: &str) -> String {
    format!(
        "{TRANSLATION_RULES}\n\nTarget language: {target_language}\nUnit ID: {unit_id}\n\n--- SOURCE ---\n{protected_source}\n--- END SOURCE ---"
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
        "{REPAIR_RULES}\n\nTarget language: {target_language}\nUnit ID: {unit_id}\n\nFindings:\n{findings}\n\n--- SOURCE ---\n{protected_source}\n--- END SOURCE ---\n\n--- CANDIDATE ---\n{candidate}\n--- END CANDIDATE ---"
    )
}
