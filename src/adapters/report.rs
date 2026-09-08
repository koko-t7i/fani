use crate::domain::model::{LanguageOutcome, Status};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn overall(outcomes: &[LanguageOutcome]) -> Status {
    [
        Status::Error,
        Status::NeedsHuman,
        Status::Partial,
        Status::Ok,
    ]
    .into_iter()
    .find(|status| outcomes.iter().any(|outcome| outcome.status == *status))
    .unwrap_or(Status::Ok)
}

pub fn build(
    outcomes: &[LanguageOutcome],
    started: DateTime<Local>,
    config: &Path,
    database_paths: &[PathBuf],
) -> Value {
    let agent_calls = outcomes.iter().flat_map(|outcome| &outcome.agent_calls);
    let calls = agent_calls.clone().count();
    let attempts = agent_calls.clone().map(|call| call.attempts).sum::<usize>();
    let failures = agent_calls.filter(|call| !call.ok).count();
    let status = overall(outcomes);
    json!({
        "schema": 4,
        "config": config.display().to_string(),
        "databases": database_paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "started_at": started.to_rfc3339(),
        "duration_s": (Local::now() - started).num_milliseconds() as f64 / 1000.0,
        "status": status.as_str(),
        "exit_code": status.exit_code(),
        "totals": {
            "languages": outcomes.len(),
            "markdown_files": outcomes.iter().map(|outcome| outcome.documents.markdown_files).sum::<usize>(),
            "mdx_files": outcomes.iter().map(|outcome| outcome.documents.mdx_files).sum::<usize>(),
            "json_files": outcomes.iter().map(|outcome| outcome.documents.json_files).sum::<usize>(),
            "parse_failures": outcomes.iter().map(|outcome| outcome.documents.parse_failures).sum::<usize>(),
            "verified_documents": outcomes.iter().map(|outcome| outcome.documents.verified_documents).sum::<usize>(),
            "pass_through_documents": outcomes.iter().map(|outcome| outcome.documents.pass_through_documents).sum::<usize>(),
            "files_written": outcomes.iter().map(|outcome| outcome.written.len()).sum::<usize>(),
            "conflicts": outcomes.iter().map(|outcome| outcome.conflicts.len()).sum::<usize>(),
            "findings": outcomes.iter().map(|outcome| outcome.findings.len()).sum::<usize>(),
            "agent_calls": calls,
            "agent_attempts": attempts,
            "agent_retries": attempts.saturating_sub(calls),
            "agent_failures": failures,
            "repair_rounds": outcomes.iter().map(|outcome| outcome.repair_rounds).sum::<usize>(),
            "remaining_tasks": outcomes.iter().map(|outcome| outcome.remaining_tasks).sum::<usize>(),
            "reused_units": outcomes.iter().map(|outcome| outcome.reused_units).sum::<usize>(),
            "commits": outcomes.iter().filter(|outcome| !outcome.published.commit.is_empty()).count(),
        },
        "languages": outcomes,
    })
}

pub fn render(data: &Value) -> String {
    let totals = &data["totals"];
    let mut lines = vec![
        format!(
            "# fani run — {}",
            data["status"].as_str().unwrap_or("error")
        ),
        String::new(),
        format!("- started: {}", data["started_at"].as_str().unwrap_or("")),
        format!(
            "- duration: {:.1}s",
            data["duration_s"].as_f64().unwrap_or(0.0)
        ),
        format!("- config: `{}`", data["config"].as_str().unwrap_or("")),
        format!(
            "- files written: {} · conflicts: {} · findings: {}",
            totals["files_written"], totals["conflicts"], totals["findings"]
        ),
        format!(
            "- Markdown: {} · MDX: {} · JSON: {} · parse failures: {} · verified: {} · pass-through: {}",
            totals["markdown_files"],
            totals["mdx_files"],
            totals["json_files"],
            totals["parse_failures"],
            totals["verified_documents"],
            totals["pass_through_documents"]
        ),
        format!(
            "- Agent calls: {} · attempts: {} · failures: {} · reused units: {}",
            totals["agent_calls"],
            totals["agent_attempts"],
            totals["agent_failures"],
            totals["reused_units"]
        ),
        String::new(),
        "| repo | language | source | status | written | findings | Agent calls | commit |".into(),
        "| --- | --- | --- | --- | ---: | ---: | ---: | --- |".into(),
    ];
    for language in data["languages"].as_array().into_iter().flatten() {
        let repo = Path::new(language["repo"].as_str().unwrap_or("?"))
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?");
        let revision = language["source_revision"].as_str().unwrap_or("");
        let revision = &revision[..revision.len().min(9)];
        let commit = language["published"]["commit"].as_str().unwrap_or("");
        let commit = if commit.is_empty() {
            "—"
        } else {
            &commit[..commit.len().min(9)]
        };
        lines.push(format!(
            "| {repo} | {} | `{revision}` | {} | {} | {} | {} | `{commit}` |",
            language["lang"].as_str().unwrap_or(""),
            language["status"].as_str().unwrap_or("error"),
            language["written"].as_array().map_or(0, Vec::len),
            language["findings"].as_array().map_or(0, Vec::len),
            language["agent_calls"].as_array().map_or(0, Vec::len),
        ));
    }
    lines.push(String::new());
    for language in data["languages"].as_array().into_iter().flatten() {
        if language["status"] == "ok" && language["findings"].as_array().is_none_or(Vec::is_empty) {
            continue;
        }
        lines.push(format!(
            "## {} — {}",
            Path::new(language["repo"].as_str().unwrap_or("?"))
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?"),
            language["lang"].as_str().unwrap_or("")
        ));
        lines.push(String::new());
        lines.push(language["message"].as_str().unwrap_or("").to_owned());
        lines.push(String::new());
        for finding in language["conflicts"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(language["findings"].as_array().into_iter().flatten())
        {
            lines.push(format!(
                "- `{}` {} {}: {}",
                finding["path"].as_str().unwrap_or("?"),
                finding["severity"].as_str().unwrap_or(""),
                finding["code"].as_str().unwrap_or(""),
                finding["message"].as_str().unwrap_or("")
            ));
        }
        for call in language["agent_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|call| call["ok"] == false)
        {
            lines.push(format!(
                "- Agent `{}` [{}]: {}",
                call["task_id"].as_str().unwrap_or("?"),
                call["code"].as_str().unwrap_or("AGENT-EXIT"),
                call["diagnostic"].as_str().unwrap_or("")
            ));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_owned() + "\n"
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(content)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn write(
    outcomes: &[LanguageOutcome],
    directory: &Path,
    started: DateTime<Local>,
    config: &Path,
    database_paths: &[PathBuf],
) -> Result<Value> {
    let data = build(outcomes, started, config, database_paths);
    fs::create_dir_all(directory)?;
    atomic_write(
        &directory.join("report.json"),
        (serde_json::to_string_pretty(&data)? + "\n").as_bytes(),
    )?;
    atomic_write(&directory.join("report.md"), render(&data).as_bytes())?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn exit_precedence_and_report_schema_are_stable() {
        let ok = LanguageOutcome::new(Path::new("/repo/docs"), "zh-CN");
        let mut partial = LanguageOutcome::new(Path::new("/repo/docs"), "ja");
        partial.status = Status::Partial;
        assert_eq!(overall(&[ok.clone(), partial.clone()]).exit_code(), 3);
        let tmp = tempdir().unwrap();
        let data = write(
            &[ok, partial],
            tmp.path(),
            Local::now(),
            Path::new("fani.toml"),
            &[],
        )
        .unwrap();
        assert_eq!(data["schema"], 4);
        assert_eq!(data["status"], "partial");
        assert!(tmp.path().join("report.md").is_file());
    }
}
