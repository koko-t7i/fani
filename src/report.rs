use crate::model::{LangOutcome, Status};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn overall(outcomes: &[LangOutcome]) -> Status {
    for status in [
        Status::Error,
        Status::NeedsHuman,
        Status::Partial,
        Status::Ok,
    ] {
        if outcomes.iter().any(|o| o.status == status) {
            return status;
        }
    }
    Status::Ok
}

pub fn build(
    outcomes: &[LangOutcome],
    started: DateTime<Local>,
    config: &Path,
    database_paths: &[PathBuf],
) -> Value {
    let dispatch: Vec<_> = outcomes.iter().flat_map(|o| o.dispatch.iter()).collect();
    json!({
        "schema": 2,
        "config": config.display().to_string(),
        "databases": database_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "started_at": started.to_rfc3339(),
        "duration_s": (Local::now() - started).num_milliseconds() as f64 / 1000.0,
        "host": std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".into()),
        "status": overall(outcomes).as_str(),
        "exit_code": overall(outcomes).exit_code(),
        "totals": {
            "languages": outcomes.len(),
            "files_written": outcomes.iter().map(|o| o.written.len()).sum::<usize>(),
            "conflicts": outcomes.iter().map(|o| o.conflicts.len()).sum::<usize>(),
            "findings": outcomes.iter().map(|o| o.findings.len()).sum::<usize>(),
            "agent_calls": dispatch.len(),
            "task_dispatches": dispatch.len(),
            "agent_attempts": dispatch.iter().map(|d| d.attempts).sum::<usize>(),
            "agent_retries": dispatch.iter().map(|d| d.attempts.saturating_sub(1)).sum::<usize>(),
            "agent_failures": dispatch.iter().filter(|d| !d.ok).count(),
            "repair_rounds": outcomes.iter().map(|o| o.repair_rounds).sum::<usize>(),
            "remaining_tasks": outcomes.iter().map(|o| o.remaining_tasks).sum::<usize>(),
            "fuzzy_matched": outcomes.iter().map(|o| o.fuzzy_matched).sum::<usize>(),
            "commits": outcomes.iter().filter(|o| !o.published.commit.is_empty()).count(),
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
        format!(
            "- started: {} on {}",
            data["started_at"].as_str().unwrap_or(""),
            data["host"].as_str().unwrap_or("")
        ),
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
            "- task dispatches: {} · agent attempts: {} ({} failed tasks, {} retries) · repair rounds: {} · reused from memory: {}",
            totals["task_dispatches"],
            totals["agent_attempts"],
            totals["agent_failures"],
            totals["agent_retries"],
            totals["repair_rounds"],
            totals["fuzzy_matched"]
        ),
        String::new(),
    ];
    let languages = data["languages"].as_array().cloned().unwrap_or_default();
    if !languages.is_empty() {
        lines.extend([
            "| repo | lang | status | written | findings | agent calls | commit | time |".into(),
            "| --- | --- | --- | --- | --- | --- | --- | --- |".into(),
        ]);
        for lang in &languages {
            let repo = Path::new(lang["repo"].as_str().unwrap_or("?"))
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("?");
            let commit = lang["published"]["commit"]
                .as_str()
                .filter(|x| !x.is_empty())
                .map(|sha| {
                    format!(
                        "`{}` on `{}`",
                        &sha[..sha.len().min(9)],
                        lang["published"]["branch"].as_str().unwrap_or("")
                    )
                })
                .unwrap_or_else(|| "—".into());
            lines.push(format!(
                "| {repo} | {} | {} | {} | {} | {} | {commit} | {:.1}s |",
                lang["lang"].as_str().unwrap_or(""),
                lang["status"].as_str().unwrap_or(""),
                lang["written"].as_array().map_or(0, Vec::len),
                lang["findings"].as_array().map_or(0, Vec::len),
                lang["dispatch"].as_array().map_or(0, Vec::len),
                lang["duration_s"].as_f64().unwrap_or(0.0)
            ));
        }
        lines.push(String::new());
    }
    for lang in &languages {
        let has_failed_calls = lang["dispatch"]
            .as_array()
            .is_some_and(|calls| calls.iter().any(|call| call["ok"] == false));
        let clean = lang["status"] == "ok"
            && lang["findings"].as_array().is_none_or(Vec::is_empty)
            && !has_failed_calls;
        if clean {
            continue;
        }
        let repo = Path::new(lang["repo"].as_str().unwrap_or("?"))
            .file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("?");
        lines.extend([
            format!("## {repo} — {}", lang["lang"].as_str().unwrap_or("")),
            String::new(),
            lang["message"].as_str().unwrap_or("").into(),
            String::new(),
        ]);
        if let Some(conflicts) = lang["conflicts"].as_array().filter(|x| !x.is_empty()) {
            lines.push("Hand-edited translations (not overwritten):".into());
            for c in conflicts {
                lines.push(format!(
                    "- `{}`",
                    c.get("path").and_then(Value::as_str).unwrap_or("?")
                ));
            }
            lines.push(String::new());
        }
        let failed_calls: Vec<_> = lang["dispatch"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|call| call["ok"] == false)
            .collect();
        if !failed_calls.is_empty() {
            lines.push("Failed agent calls:".into());
            for call in failed_calls {
                lines.push(format!(
                    "- `{}` [{}]: {}",
                    call["task_id"].as_str().unwrap_or("?"),
                    call["code"].as_str().unwrap_or("DSP-EXIT"),
                    call["message"].as_str().unwrap_or("")
                ));
            }
            lines.push(String::new());
        }
        if let Some(findings) = lang["findings"].as_array().filter(|x| !x.is_empty()) {
            lines.push("Findings:".into());
            for f in findings.iter().take(40) {
                lines.push(format!(
                    "- `{}` {} {}: {}",
                    f.get("file")
                        .or_else(|| f.get("path"))
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    f["severity"].as_str().unwrap_or(""),
                    f["code"].as_str().unwrap_or(""),
                    f["message"].as_str().unwrap_or("")
                ));
            }
            if findings.len() > 40 {
                lines.push(format!(
                    "- … {} more (see report.json)",
                    findings.len() - 40
                ));
            }
            lines.push(String::new());
        }
    }
    lines.join("\n").trim_end().to_string() + "\n"
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn write(
    outcomes: &[LangOutcome],
    dir: &Path,
    started: DateTime<Local>,
    config: &Path,
    database_paths: &[PathBuf],
) -> Result<Value> {
    let data = build(outcomes, started, config, database_paths);
    fs::create_dir_all(dir)?;
    atomic_write(
        &dir.join("report.json"),
        (serde_json::to_string_pretty(&data)? + "\n").as_bytes(),
    )?;
    atomic_write(&dir.join("report.md"), render(&data).as_bytes())?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LangOutcome;
    use tempfile::tempdir;

    #[test]
    fn worst_status_and_reports_match_contract() {
        let mut ok = LangOutcome::new(Path::new("/repo/docs"), "zh-CN");
        ok.message = "done".into();
        let mut partial = LangOutcome::new(Path::new("/repo/docs"), "ja");
        partial.status = Status::Partial;
        partial.remaining_tasks = 2;
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
        assert_eq!(data["status"], "partial");
        assert!(tmp.path().join("report.json").is_file());
        assert!(
            fs::read_to_string(tmp.path().join("report.md"))
                .unwrap()
                .starts_with("# fani run")
        );
    }
}
