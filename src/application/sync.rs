pub(crate) mod context;
pub(crate) mod materialization;
pub mod planning;
pub(crate) mod preparation;
mod publication;
mod reconciliation;
pub(crate) mod recovery;
pub(crate) mod repair;
pub(crate) mod review;
pub(crate) mod translation;
pub(crate) mod types;
pub(crate) mod verification;
use crate::application::command::OutputReporter;
use crate::application::ports::{
    AgentExecutor, CodeHost, DocumentationChecker, GitPublisher, Materializer, StateStore,
};
use crate::application::ports::{SourceReader, TargetReader};
use crate::application::settings::RepoConfig;
use crate::application::sync::materialization::MaterializationService;
use crate::application::sync::planning::Planner;
use crate::application::sync::preparation::PreparationService;
use crate::application::sync::publication::PublicationService;
use crate::application::sync::repair::RepairService;
use crate::application::sync::review::ReviewService;
use crate::application::sync::translation::TranslationService;
use crate::application::sync::verification::VerificationService;
use crate::domain::model::{FindingSeverity, LanguageOutcome, PlanSummary, Status};
use crate::domain::prompts;
use anyhow::Result;
pub use reconciliation::{adopt_human_edit, discard_human_edit};
use std::path::Path;
use std::time::Instant;

use crate::application::sync::context::hash;

pub struct Orchestrator<'a> {
    repo: &'a RepoConfig,
    database: &'a dyn StateStore,
    materializer: &'a dyn Materializer,
    agents: &'a dyn AgentExecutor,
    documentation: &'a dyn DocumentationChecker,
    git: &'a dyn GitPublisher,
    code_host: &'a dyn CodeHost,
    config_path: &'a Path,
    owner_identity: &'a str,
    failpoint: fn(&str),
    output: &'a dyn OutputReporter,
    quiet: bool,
    source_reader: &'a dyn SourceReader,
    target_reader: &'a dyn TargetReader,
}

impl<'a> Orchestrator<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: &'a RepoConfig,
        database: &'a dyn StateStore,
        materializer: &'a dyn Materializer,
        agents: &'a dyn AgentExecutor,
        documentation: &'a dyn DocumentationChecker,
        git: &'a dyn GitPublisher,
        code_host: &'a dyn CodeHost,
        config_path: &'a Path,
        owner_identity: &'a str,
        failpoint: fn(&str),
        output: &'a dyn OutputReporter,
        quiet: bool,
        source_reader: &'a dyn SourceReader,
        target_reader: &'a dyn TargetReader,
    ) -> Self {
        Self {
            repo,
            database,
            materializer,
            agents,
            documentation,
            git,
            code_host,
            config_path,
            owner_identity,
            failpoint,
            output,
            quiet,
            source_reader,
            target_reader,
        }
    }

    fn traced_stage<T>(
        &self,
        language: &str,
        run_id: &str,
        stage: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let started = Instant::now();
        let repository_id =
            crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes());
        let run_id = crate::diagnostics::safe_id(run_id);
        tracing::info!(
            event = "stage.started",
            repository_id,
            run_id,
            locale = language,
            stage,
        );
        let result = operation();
        tracing::info!(
            event = "stage.completed",
            repository_id,
            run_id,
            locale = language,
            stage,
            status = if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            },
            duration_ms = started.elapsed().as_millis() as u64,
        );
        result
    }

    fn repository_id(&self) -> Result<i64> {
        let key = self
            .repo
            .path
            .canonicalize()
            .unwrap_or_else(|_| self.repo.path.clone());
        self.database.repositories().upsert_repository(
            &hash(&[key.to_string_lossy().as_bytes()]),
            &self.repo.path,
            Some(&self.repo.publish.github.base),
            None,
        )
    }

    fn planner(&self) -> Result<Planner<'_>> {
        Ok(Planner {
            repo: self.repo,
            database: self.database.planning(),
            materializer: self.target_reader,
            git: self.source_reader,
            agent_fingerprint: self.agents.configuration_fingerprint()?,
        })
    }

    pub fn plan_language(&self, language: &str) -> Result<PlanSummary> {
        self.planner()?.plan_language(language)
    }

    fn publication(&self) -> PublicationService<'_> {
        PublicationService {
            repo: self.repo,
            store: self.database.publication(),
            git: self.git,
            code_host: self.code_host,
            documentation: self.documentation,
            owner_identity: self.owner_identity,
            failpoint: self.failpoint,
        }
    }

    fn materialization(&self) -> MaterializationService<'_> {
        MaterializationService {
            repo: self.repo,
            database: self.database.materialization(),
            materializer: self.materializer,
            git: self.source_reader,
            owner_identity: self.owner_identity,
            failpoint: self.failpoint,
        }
    }

    pub fn run_language(&self, language: &str) -> LanguageOutcome {
        let started = Instant::now();
        let repository_id =
            crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes());
        tracing::info!(
            event = "locale.run.started",
            repository_id,
            locale = language,
        );
        let mut outcome = LanguageOutcome::new(&self.repo.path, language);
        let result = (|| -> Result<()> {
            let source_revision = self.git.resolve_source_revision(self.repo)?;
            outcome.source_revision = source_revision.clone();
            let repository_id = self.repository_id()?;
            self.materialization()
                .retire_incompatible_materializations(repository_id, language, &source_revision)?;
            self.publication()
                .reconcile_pull_request(repository_id, language)?;
            let policy_fingerprint = prompts::policy_fingerprint();
            let invocation =
                self.planner()?
                    .invocation_key(repository_id, language, &source_revision)?;
            let run_id = self.database.runs().begin_run(
                repository_id,
                &invocation,
                self.config_path,
                "{}",
                &policy_fingerprint,
            )?;
            outcome.run_id = run_id.clone();
            tracing::info!(
                event = "run.started",
                repository_id = %crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes()),
                run_id = %crate::diagnostics::safe_id(&run_id),
                locale = language,
                source_revision_id = %crate::diagnostics::safe_id(&source_revision),
                policy_id = %crate::diagnostics::safe_id(&policy_fingerprint),
            );
            if self.repo.publish.enabled {
                outcome.transitions.push("recovering:publication".into());
                self.traced_stage(language, &run_id, "publication_recovery", || {
                    self.publication()
                        .publish(repository_id, &run_id, language, &[], &mut outcome)
                })?;
                if !outcome.published.commit.is_empty() {
                    if outcome.documents.parse_failures > 0 {
                        outcome.status = Status::NeedsHuman;
                        outcome.message =
                            "source document parse failures require human attention".into();
                        outcome.transitions.push("needs_human:conflict".into());
                    } else {
                        outcome.message = "recovered pending publication".into();
                        outcome.transitions.push("complete:ok".into());
                    }
                    return Ok(());
                }
            }
            let (mut documents, conflicts, reused, scheduled) =
                self.traced_stage(language, &run_id, "planning", || {
                    PreparationService {
                        repo: self.repo,
                        database: self.database.preparation(),
                        git: self.source_reader,
                        planner: self.planner()?,
                    }
                    .prepare(
                        repository_id,
                        &run_id,
                        language,
                        &source_revision,
                        &mut outcome.documents,
                    )
                })?;
            outcome.reused_units = reused;
            outcome.conflicts = conflicts;
            if !outcome.conflicts.is_empty() {
                outcome.status = Status::NeedsHuman;
                outcome.message = format!(
                    "{} conflict(s) require adopt/discard or unit disambiguation",
                    outcome.conflicts.len()
                );
                outcome.transitions.push("needs_human:conflict".into());
                return Ok(());
            }
            let pending_total = documents
                .iter()
                .flat_map(|document| &document.units)
                .filter(|unit| unit.translation.is_none())
                .count();
            outcome.remaining_tasks = pending_total.saturating_sub(scheduled);
            if scheduled > 0 {
                outcome.transitions.push("dispatching".into());
                outcome.agent_calls = self.traced_stage(language, &run_id, "translate", || {
                    TranslationService {
                        database: self.database.pipeline(),
                        agents: self.agents,
                        failpoint: self.failpoint,
                        output: self.output,
                        quiet: self.quiet,
                    }
                    .dispatch(&run_id, language, &mut documents)
                })?;
                if documents
                    .iter()
                    .flat_map(|document| &document.units)
                    .any(|unit| unit.translation.is_none() && unit.work_item_id.is_some())
                {
                    outcome.transitions.push("repairing".into());
                    outcome.repair_rounds =
                        self.traced_stage(language, &run_id, "repair", || {
                            RepairService {
                                repo: self.repo,
                                database: self.database.pipeline(),
                                agents: self.agents,
                                failpoint: self.failpoint,
                            }
                            .repair(
                                language,
                                &mut documents,
                                &mut outcome.agent_calls,
                            )
                        })?;
                }
            }
            outcome.transitions.push("reviewing".into());
            let mut review_findings = self.traced_stage(language, &run_id, "review", || {
                ReviewService {
                    repo: self.repo,
                    database: self.database.pipeline(),
                    agents: self.agents,
                    failpoint: self.failpoint,
                }
                .review(language, &mut documents, &mut outcome.agent_calls)
            })?;
            let review_blocked = review_findings
                .iter()
                .any(|finding| finding.severity == FindingSeverity::Error);
            outcome.findings.append(&mut review_findings);
            if review_blocked {
                outcome.status = Status::NeedsHuman;
                outcome.message = "blocking bilingual revision did not pass".into();
                outcome.transitions.push("needs_human:revision".into());
                return Ok(());
            }
            outcome.transitions.push("verifying".into());
            let verified = self.traced_stage(language, &run_id, "verification", || {
                VerificationService {
                    repo: self.repo,
                    database: self.database.verification(),
                    documentation: self.documentation,
                }
                .verify(&run_id, language, &source_revision, &documents)
            })?;
            outcome.documents.verified_documents = verified.verified_documents;
            outcome.findings.extend(verified.findings);
            let Some(verified) = verified.batch else {
                outcome.status = Status::NeedsHuman;
                outcome.message = "deterministic findings block publication".into();
                outcome.transitions.push("needs_human:verification".into());
                return Ok(());
            };
            let (written, candidates, mut findings) =
                self.traced_stage(language, &run_id, "materialization", || {
                    self.materialization().materialize(
                        repository_id,
                        &run_id,
                        language,
                        &source_revision,
                        &verified,
                    )
                })?;
            outcome.written = written;
            outcome.documents.verified_documents = candidates.len();
            outcome.findings.append(&mut findings);
            if outcome
                .findings
                .iter()
                .any(|finding| finding.severity == FindingSeverity::Error)
            {
                outcome.status = Status::NeedsHuman;
                outcome.message = "deterministic findings block publication".into();
                outcome.transitions.push("needs_human:verification".into());
                return Ok(());
            }
            if outcome.remaining_tasks > 0 {
                outcome.status = Status::Partial;
                outcome.message = format!(
                    "verified this bounded batch; {} unit(s) remain",
                    outcome.remaining_tasks
                );
            } else if scheduled == 0 && outcome.written.is_empty() {
                outcome.message = "every translation is up to date".into();
            } else {
                outcome.message =
                    format!("verified {} translated document(s)", outcome.written.len());
            }
            outcome.transitions.push("publishing".into());
            let eligible = candidates
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>();
            self.traced_stage(language, &run_id, "publication", || {
                self.publication().publish(
                    repository_id,
                    &run_id,
                    language,
                    &eligible,
                    &mut outcome,
                )
            })?;
            outcome
                .transitions
                .push(format!("complete:{}", outcome.status.as_str()));
            Ok(())
        })();
        if let Err(error) = result {
            outcome.status = Status::Error;
            outcome.message = error.to_string();
            outcome.transitions.push("error".into());
        }
        if !outcome.run_id.is_empty() {
            if let Err(error) = self
                .database
                .runs()
                .finish_run(&outcome.run_id, outcome.status.as_str())
            {
                outcome.status = Status::Error;
                outcome.message = format!("cannot finalize durable run: {error}");
                outcome.transitions.push("error:run_finalize".into());
            }
        }
        outcome.duration_s = started.elapsed().as_secs_f64();
        tracing::info!(
            event = "run.completed",
            repository_id = %crate::diagnostics::safe_id(self.repo.path.to_string_lossy().as_bytes()),
            run_id = %crate::diagnostics::safe_id(&outcome.run_id),
            locale = language,
            status = outcome.status.as_str(),
            duration_ms = (outcome.duration_s * 1000.0) as u64,
            agent_calls = outcome.agent_calls.len(),
            files_written = outcome.written.len(),
            findings = outcome.findings.len(),
            conflicts = outcome.conflicts.len(),
            remaining_tasks = outcome.remaining_tasks,
            publication_id = %crate::diagnostics::safe_id(&outcome.published.commit),
            publication_pushed = outcome.published.pushed,
        );
        outcome
    }
}
