use crate::agent::Dispatcher;
use crate::config::{Config, RepoConfig};
use crate::db::Database;
use crate::gitout::{publish, publish_pending};
use crate::model::{LangOutcome, Status, TaskOutcome, VerifyResult};
use crate::skill::{Skill, SkillApi, findings_for_tasks, slug, task_files};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Instant;

pub struct Orchestrator<'a> {
    cfg: &'a Config,
    repo: &'a RepoConfig,
    db: &'a Database,
    db_run_id: &'a str,
    quiet: bool,
}

impl<'a> Orchestrator<'a> {
    pub fn new(
        cfg: &'a Config,
        repo: &'a RepoConfig,
        db: &'a Database,
        db_run_id: &'a str,
        quiet: bool,
    ) -> Self {
        Self {
            cfg,
            repo,
            db,
            db_run_id,
            quiet,
        }
    }

    fn log(&self, text: &str) {
        if !self.quiet {
            eprintln!("{text}");
        }
    }

    pub fn run_language(&self, lang: &str) -> LangOutcome {
        let skill = Skill::new(&self.cfg.skill, &self.repo.path, &self.repo.state_dir);
        let result = self.run_language_with(lang, &skill, |stage, kind, work, findings| {
            let agent = self.cfg.agent_for(stage)?;
            let tasks = task_files(work, kind);
            if tasks.is_empty() {
                return Ok(vec![]);
            }
            self.log(&format!(
                "    dispatching {} {kind} to {} (concurrency {})",
                tasks.len(),
                agent.name,
                agent.concurrency
            ));
            let repo = self.repo.path.display().to_string();
            Dispatcher::new(&self.repo.path, agent).run_recording(
                &tasks,
                kind,
                &findings,
                |outcome| {
                    self.db.record_agent_calls(
                        self.db_run_id,
                        &repo,
                        lang,
                        stage,
                        &agent.name,
                        std::slice::from_ref(outcome),
                    )
                },
            )
        });
        match result {
            Ok(out) => out,
            Err(err) => {
                let mut out = LangOutcome::new(&self.repo.path, lang);
                out.status = Status::Error;
                out.message = err.to_string();
                out.transitions.push("error".into());
                out
            }
        }
    }

    pub fn run_language_with<F>(
        &self,
        lang: &str,
        skill: &dyn SkillApi,
        mut dispatch: F,
    ) -> Result<LangOutcome>
    where
        F: FnMut(&str, &str, &Path, HashMap<String, String>) -> Result<Vec<TaskOutcome>>,
    {
        let started = Instant::now();
        let mut out = LangOutcome::new(&self.repo.path, lang);
        self.log(&format!(
            "  {} [{lang}] planning",
            self.repo
                .path
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or("repo")
        ));
        let plan = skill
            .plan(
                lang,
                &self.repo.paths,
                &self.repo.exclude,
                self.repo.max_tasks,
                None,
            )?
            .data;
        out.run_id = plan.run_id.clone();
        out.fuzzy_matched = plan.fuzzy_matched;
        out.remaining_tasks = plan.truncated_tasks;
        let work = if plan.run_id.is_empty() {
            self.repo.path.clone()
        } else {
            skill.work_dir(&plan.run_id)
        };

        if !plan.conflicts.is_empty() {
            out.status = Status::NeedsHuman;
            out.conflicts = plan.conflicts;
            out.message = format!(
                "{} translation(s) were edited by hand; fani will not overwrite them",
                out.conflicts.len()
            );
            out.transitions.push("needs_human:conflict".into());
            return Ok(finish(out, started));
        }
        if plan.task_count == 0 {
            let repo = self.repo.path.display().to_string();
            let pending = if self.repo.commit && self.repo.push {
                self.db.pending_push(&repo, lang)?
            } else {
                None
            };
            if let Some(pending) = pending {
                out.published = match publish_pending(self.repo, lang, &pending.commit) {
                    Ok(published) => published,
                    Err(error) => {
                        out.status = Status::NeedsHuman;
                        out.message =
                            format!("translations are current, but publication failed: {error}");
                        out.transitions.push("needs_human:publish".into());
                        return Ok(finish(out, started));
                    }
                };
                if !out.published.error.is_empty() {
                    out.status = Status::NeedsHuman;
                    out.message = out.published.error.clone();
                    out.transitions.push("needs_human:push".into());
                } else if out.published.pushed {
                    out.message = format!("pushed pending commit {}", out.published.commit);
                    out.transitions.push("complete:pending_push".into());
                }
            }
            if out.published.commit.is_empty() {
                out.message = "every translation is up to date".into();
                out.transitions.push("complete:no_changes".into());
            }
            return Ok(finish(out, started));
        }
        if let Some(message) = self.guard_tripped(skill, plan.task_count)? {
            out.status = Status::NeedsHuman;
            out.message = message;
            out.transitions
                .push("needs_human:full_retranslate_guard".into());
            return Ok(finish(out, started));
        }

        backup_state(skill, &work)?;
        out.transitions.push("dispatching".into());
        let first = dispatch("translate", "tasks", &work, HashMap::new())?;
        let failed = first.iter().any(|x| !x.ok);
        out.dispatch.extend(first);
        if failed {
            out.status = Status::NeedsHuman;
            out.message = "one or more agent calls failed; no translation was applied".into();
            out.transitions.push("needs_human:dispatch_failed".into());
            return Ok(finish(out, started));
        }

        out.transitions.push("applying".into());
        let mut applied = skill.apply(&out.run_id)?.data;
        if !applied.rejected.is_empty() {
            out.transitions.push("repairing:assembly".into());
            let rejected_files: Vec<String> = applied
                .rejected
                .iter()
                .filter_map(|r| r.get("file").and_then(Value::as_str).map(str::to_string))
                .collect();
            let redo: Vec<_> = task_files(&work, "tasks")
                .into_iter()
                .filter(|task| {
                    let stem = task.file_stem().and_then(|x| x.to_str()).unwrap_or("");
                    rejected_files
                        .iter()
                        .any(|file| stem.starts_with(&slug(file)))
                })
                .collect();
            let redo_work = work.join("redo");
            // The dispatcher accepts a work directory, while the selected task list may be sparse.
            // Link only rejected tasks into a short-lived protocol directory.
            if redo_work.exists() {
                fs::remove_dir_all(&redo_work)?;
            }
            fs::create_dir_all(redo_work.join("tasks"))?;
            for task in redo {
                let target = redo_work.join("tasks").join(task.file_name().unwrap());
                fs::copy(task, target)?;
            }
            let retried = dispatch("translate", "tasks", &redo_work, HashMap::new())?;
            let retry_failed = retried.iter().any(|x| !x.ok);
            out.dispatch.extend(retried);
            if retry_failed {
                out.status = Status::NeedsHuman;
                out.findings = applied.rejected;
                out.message = "assembly repair agent call failed".into();
                out.transitions
                    .push("needs_human:assembly_dispatch_failed".into());
                return Ok(finish(out, started));
            }
            applied = skill.apply(&out.run_id)?.data;
        }
        out.written.extend(applied.written);
        if !applied.rejected.is_empty() {
            out.status = Status::NeedsHuman;
            out.findings = applied.rejected;
            out.message = format!(
                "{} file(s) could not be assembled after a retry",
                out.findings.len()
            );
            out.transitions.push("needs_human:assembly_rejected".into());
            return Ok(finish(out, started));
        }

        out.transitions.push("verifying".into());
        let mut verify = skill.verify(lang)?.data;
        validate_verify(&verify)?;
        while verify.status == "fail" && out.repair_rounds < self.repo.repair_budget {
            out.repair_rounds += 1;
            out.transitions
                .push(format!("repairing:verify:{}", out.repair_rounds));
            let verify_path = work.join("verify.json");
            fs::create_dir_all(verify_path.parent().unwrap())?;
            fs::write(
                &verify_path,
                serde_json::to_string(&serde_json::json!({
                    "status": verify.status, "findings": verify.findings, "retry_files": verify.retry_files
                }))?,
            )?;
            let repair_plan = skill
                .plan(
                    lang,
                    &self.repo.paths,
                    &self.repo.exclude,
                    self.repo.max_tasks,
                    Some(&verify_path),
                )?
                .data;
            if repair_plan.task_count == 0 {
                break;
            }
            let repair_work = skill.work_dir(&repair_plan.run_id);
            let findings = findings_for_tasks(&verify, &task_files(&repair_work, "tasks"));
            let repaired = dispatch("translate", "tasks", &repair_work, findings)?;
            let repair_failed = repaired.iter().any(|x| !x.ok);
            out.dispatch.extend(repaired);
            if repair_failed {
                break;
            }
            let repaired_apply = skill.apply(&repair_plan.run_id)?.data;
            out.written.extend(repaired_apply.written);
            if !repaired_apply.rejected.is_empty() {
                verify = VerifyResult {
                    status: "fail".into(),
                    findings: repaired_apply.rejected,
                    retry_files: vec![],
                };
                break;
            }
            verify = skill.verify(lang)?.data;
            validate_verify(&verify)?;
        }
        out.findings = verify.findings.clone();
        if verify.status == "fail" {
            out.status = Status::NeedsHuman;
            out.message = format!(
                "verification still failing after {} repair round(s): {}",
                out.repair_rounds,
                if verify.retry_files.is_empty() {
                    "see findings".into()
                } else {
                    verify.retry_files.join(", ")
                }
            );
            out.transitions.push("needs_human:verification".into());
            return Ok(finish(out, started));
        }

        if self.repo.stages.revision {
            self.review(lang, "revision", true, skill, &mut dispatch, &mut out)?;
            if out.status != Status::Ok {
                return Ok(finish(out, started));
            }
        }
        if self.repo.stages.proofread {
            self.review(lang, "proofread", false, skill, &mut dispatch, &mut out)?;
        }

        out.transitions.push("publishing".into());
        out.published = match publish(self.repo, lang, &out.written) {
            Ok(published) => published,
            Err(error) => {
                out.status = Status::NeedsHuman;
                out.message = format!("translated, but could not commit: {error}");
                out.transitions.push("needs_human:publish".into());
                return Ok(finish(out, started));
            }
        };
        if !out.published.error.is_empty() {
            out.status = Status::NeedsHuman;
            out.message = format!(
                "translated and committed as {}, but {}",
                out.published.commit, out.published.error
            );
            out.transitions.push("needs_human:push".into());
            return Ok(finish(out, started));
        }
        if out.remaining_tasks > 0 {
            out.status = Status::Partial;
            out.message = format!(
                "{} chunk(s) exceeded max_tasks and were not planned; the next run continues where this one stopped",
                out.remaining_tasks
            );
            out.transitions.push("complete:partial".into());
        } else {
            out.message = format!("wrote {} file(s)", out.written.len());
            out.transitions.push("complete:ok".into());
        }
        Ok(finish(out, started))
    }

    fn review<F>(
        &self,
        lang: &str,
        mode: &str,
        blocking: bool,
        skill: &dyn SkillApi,
        dispatch: &mut F,
        out: &mut LangOutcome,
    ) -> Result<()>
    where
        F: FnMut(&str, &str, &Path, HashMap<String, String>) -> Result<Vec<TaskOutcome>>,
    {
        out.transitions.push(format!("reviewing:{mode}"));
        let plan = skill.review_plan(lang, mode, Some(&out.run_id))?;
        if plan.returncode == 3 || plan.data.task_count == 0 {
            return Ok(());
        }
        let work = skill.work_dir(&plan.data.run_id);
        let calls = dispatch(mode, "review", &work, HashMap::new())?;
        let failed = calls.iter().any(|x| !x.ok);
        out.dispatch.extend(calls);
        if failed && blocking {
            out.status = Status::NeedsHuman;
            out.message = format!("{mode} agent call failed");
            out.transitions.push(format!("needs_human:{mode}_dispatch"));
            return Ok(());
        }
        if failed {
            return Ok(());
        }
        let collected = skill.review_collect(&plan.data.run_id)?.data;
        let blocking_findings = collected
            .findings
            .iter()
            .filter(|f| f.get("severity").and_then(Value::as_str) == Some("error"))
            .count();
        out.findings.extend(collected.findings);
        if blocking && blocking_findings > 0 {
            out.status = Status::NeedsHuman;
            out.message = format!("revision found {blocking_findings} blocking issue(s)");
            out.transitions.push("needs_human:revision".into());
        }
        Ok(())
    }

    fn guard_tripped(&self, skill: &dyn SkillApi, task_count: usize) -> Result<Option<String>> {
        let limit = self.repo.full_retranslate_guard;
        if limit == 0 || task_count <= limit || !skill.state_path().is_file() {
            return Ok(None);
        }
        let value: Value =
            serde_json::from_str(&fs::read_to_string(skill.state_path())?).unwrap_or_default();
        let known = value
            .get("files")
            .and_then(Value::as_object)
            .map_or(0, |x| x.len());
        if known == 0 {
            return Ok(None);
        }
        Ok(Some(format!(
            "{task_count} fresh chunks exceeds full_retranslate_guard ({limit}) while state.json still records {known} file(s). Review the plan, then raise the guard if expected."
        )))
    }
}

fn validate_verify(verify: &VerifyResult) -> Result<()> {
    if verify.status == "pass" || verify.status == "fail" {
        return Ok(());
    }
    anyhow::bail!("verify returned unknown status {:?}", verify.status)
}

fn finish(mut out: LangOutcome, started: Instant) -> LangOutcome {
    out.duration_s = started.elapsed().as_secs_f64();
    out
}

pub fn backup_state(skill: &dyn SkillApi, work: &Path) -> Result<Option<std::path::PathBuf>> {
    let source = skill.state_path();
    if !source.is_file() {
        return Ok(None);
    }
    fs::create_dir_all(work)?;
    let dest = work.join("state.json.backup");
    fs::copy(source, &dest).context("cannot back up external skill state")?;
    Ok(Some(dest))
}
