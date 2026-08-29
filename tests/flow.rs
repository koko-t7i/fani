use fani::config::{AgentConfig, Config, RepoConfig, RepoStages};
use fani::db::Database;
use fani::model::{
    ApplyResult, PlanResult, ReviewCollectResult, ReviewPlanResult, Status, TaskOutcome,
    VerifyResult,
};
use fani::orchestrator::Orchestrator;
use fani::skill::{SkillApi, SkillResult};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::tempdir;

struct StubSkill {
    root: PathBuf,
    plans: Mutex<VecDeque<PlanResult>>,
    applies: Mutex<VecDeque<ApplyResult>>,
    verifies: Mutex<VecDeque<VerifyResult>>,
    review_plans: Mutex<VecDeque<SkillResult<ReviewPlanResult>>>,
    reviews: Mutex<VecDeque<ReviewCollectResult>>,
    calls: Mutex<Vec<String>>,
}

impl StubSkill {
    fn new(
        root: &Path,
        plans: Vec<PlanResult>,
        applies: Vec<ApplyResult>,
        verifies: Vec<VerifyResult>,
    ) -> Self {
        Self {
            root: root.into(),
            plans: Mutex::new(plans.into()),
            applies: Mutex::new(applies.into()),
            verifies: Mutex::new(verifies.into()),
            review_plans: Mutex::new(VecDeque::new()),
            reviews: Mutex::new(VecDeque::new()),
            calls: Mutex::new(vec![]),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl SkillApi for StubSkill {
    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }
    fn work_dir(&self, run_id: &str) -> PathBuf {
        self.root.join("work").join(run_id)
    }
    fn plan(
        &self,
        _lang: &str,
        _paths: &[String],
        _exclude: &[String],
        _max_tasks: usize,
        repair: Option<&Path>,
    ) -> anyhow::Result<SkillResult<PlanResult>> {
        self.calls.lock().unwrap().push(
            if repair.is_some() {
                "plan-repair"
            } else {
                "plan"
            }
            .into(),
        );
        Ok(SkillResult {
            returncode: 0,
            data: self.plans.lock().unwrap().pop_front().unwrap(),
        })
    }
    fn apply(&self, _run_id: &str) -> anyhow::Result<SkillResult<ApplyResult>> {
        self.calls.lock().unwrap().push("apply".into());
        Ok(SkillResult {
            returncode: 0,
            data: self.applies.lock().unwrap().pop_front().unwrap(),
        })
    }
    fn verify(&self, _lang: &str) -> anyhow::Result<SkillResult<VerifyResult>> {
        self.calls.lock().unwrap().push("verify".into());
        Ok(SkillResult {
            returncode: 0,
            data: self.verifies.lock().unwrap().pop_front().unwrap(),
        })
    }
    fn review_plan(
        &self,
        _lang: &str,
        mode: &str,
        _run_id: Option<&str>,
    ) -> anyhow::Result<SkillResult<ReviewPlanResult>> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("review-plan:{mode}"));
        Ok(self
            .review_plans
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(SkillResult {
                returncode: 3,
                data: ReviewPlanResult::default(),
            }))
    }
    fn review_collect(&self, _run_id: &str) -> anyhow::Result<SkillResult<ReviewCollectResult>> {
        self.calls.lock().unwrap().push("review-collect".into());
        Ok(SkillResult {
            returncode: 0,
            data: self.reviews.lock().unwrap().pop_front().unwrap_or_default(),
        })
    }
}

fn plan(id: &str, count: usize) -> PlanResult {
    PlanResult {
        run_id: id.into(),
        task_count: count,
        conflicts: vec![],
        files: vec![],
        fuzzy_matched: 0,
        truncated_tasks: 0,
    }
}

fn setup(root: &Path, stages: RepoStages) -> (Config, RepoConfig, Database, String) {
    let repo = RepoConfig {
        path: root.into(),
        languages: vec!["zh-CN".into()],
        paths: vec![],
        exclude: vec![],
        state_dir: ".state".into(),
        max_tasks: 40,
        repair_budget: 2,
        full_retranslate_guard: 30,
        branch: "i18n/{lang}".into(),
        commit: false,
        push: false,
        remote: "origin".into(),
        stages,
    };
    let mut agents = HashMap::new();
    agents.insert(
        "fake".into(),
        AgentConfig {
            name: "fake".into(),
            cmd: vec!["true".into()],
            stages: vec!["translate".into(), "revision".into(), "proofread".into()],
            concurrency: 2,
            timeout_s: 1.0,
            retries: 0,
            enabled: true,
        },
    );
    let cfg = Config {
        skill: root.into(),
        repos: vec![repo.clone()],
        agents,
        routing: HashMap::from([
            ("translate".into(), "fake".into()),
            ("revision".into(), "fake".into()),
            ("proofread".into(), "fake".into()),
        ]),
    };
    let db = Database::open(root.join("fani.db")).unwrap();
    let run = db.start_run(Path::new("fani.toml")).unwrap();
    (cfg, repo, db, run)
}

fn ok_task() -> TaskOutcome {
    TaskOutcome {
        task_id: "t1".into(),
        ok: true,
        code: None,
        attempts: 1,
        duration_s: 0.01,
        message: String::new(),
    }
}

#[test]
fn happy_path_has_no_repair_or_review_transition() {
    let tmp = tempdir().unwrap();
    let (cfg, repo, db, run) = setup(tmp.path(), RepoStages::default());
    let skill = StubSkill::new(
        tmp.path(),
        vec![plan("r1", 1)],
        vec![ApplyResult {
            written: vec![json!({"path":"docs/a.zh-CN.md"})],
            rejected: vec![],
        }],
        vec![VerifyResult {
            status: "pass".into(),
            ..Default::default()
        }],
    );
    let mut dispatches = vec![];
    let out = Orchestrator::new(&cfg, &repo, &db, &run, true)
        .run_language_with("zh-CN", &skill, |stage, kind, _, _| {
            dispatches.push(format!("{stage}:{kind}"));
            Ok(vec![ok_task()])
        })
        .unwrap();
    assert_eq!(out.status, Status::Ok);
    assert_eq!(skill.calls(), vec!["plan", "apply", "verify"]);
    assert_eq!(dispatches, vec!["translate:tasks"]);
    assert_eq!(out.repair_rounds, 0);
    assert!(
        !out.transitions
            .iter()
            .any(|x| x.starts_with("repairing") || x.starts_with("reviewing"))
    );
    println!(
        "FLOW_AFTER normal skill_calls={} agent_dispatches={} transitions={}",
        skill.calls().join("->"),
        dispatches.join("->"),
        out.transitions.join("->")
    );
}

#[test]
fn conflicts_and_full_retranslate_guard_stop_before_dispatch() {
    let tmp = tempdir().unwrap();
    let (cfg, repo, db, run) = setup(tmp.path(), RepoStages::default());
    let mut conflict_plan = plan("r1", 2);
    conflict_plan.conflicts = vec![json!({"path":"docs/a.zh-CN.md"})];
    let conflict_skill = StubSkill::new(tmp.path(), vec![conflict_plan], vec![], vec![]);
    let out = Orchestrator::new(&cfg, &repo, &db, &run, true)
        .run_language_with("zh-CN", &conflict_skill, |_, _, _, _| {
            panic!("must not dispatch")
        })
        .unwrap();
    assert_eq!(out.status, Status::NeedsHuman);
    assert_eq!(conflict_skill.calls(), vec!["plan"]);

    fs::write(
        tmp.path().join("state.json"),
        "{\"files\":{\"docs/a.md\":{}}}",
    )
    .unwrap();
    let mut guarded_repo = repo.clone();
    guarded_repo.full_retranslate_guard = 1;
    let guard_skill = StubSkill::new(tmp.path(), vec![plan("r2", 2)], vec![], vec![]);
    let out = Orchestrator::new(&cfg, &guarded_repo, &db, &run, true)
        .run_language_with("zh-CN", &guard_skill, |_, _, _, _| {
            panic!("must not dispatch")
        })
        .unwrap();
    assert_eq!(out.status, Status::NeedsHuman);
    assert!(out.message.contains("full_retranslate_guard"));
}

#[test]
fn exhausted_dispatch_stops_before_apply_and_verify() {
    let tmp = tempdir().unwrap();
    let (cfg, repo, db, run) = setup(tmp.path(), RepoStages::default());
    let skill = StubSkill::new(tmp.path(), vec![plan("r1", 1)], vec![], vec![]);
    let failed = TaskOutcome {
        task_id: "t1".into(),
        ok: false,
        code: Some("DSP-EXIT".into()),
        attempts: 2,
        duration_s: 0.1,
        message: "failed".into(),
    };
    let mut dispatches = vec![];
    let out = Orchestrator::new(&cfg, &repo, &db, &run, true)
        .run_language_with("zh-CN", &skill, |stage, kind, _, _| {
            dispatches.push(format!("{stage}:{kind}"));
            Ok(vec![failed.clone()])
        })
        .unwrap();
    assert_eq!(out.status, Status::NeedsHuman);
    assert_eq!(skill.calls(), vec!["plan"]);
    assert_eq!(dispatches, vec!["translate:tasks"]);
    assert!(
        out.transitions
            .contains(&"needs_human:dispatch_failed".into())
    );
    println!(
        "FLOW_AFTER dispatch_failure skill_calls={} agent_dispatches={} transitions={}",
        skill.calls().join("->"),
        dispatches.join("->"),
        out.transitions.join("->")
    );
}

#[test]
fn verify_failure_enters_one_bounded_repair_then_recovers() {
    let tmp = tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("work/r2/tasks")).unwrap();
    fs::write(tmp.path().join("work/r2/tasks/docs-a-md.chunk.json"), "{}").unwrap();
    let (cfg, repo, db, run) = setup(tmp.path(), RepoStages::default());
    let skill = StubSkill::new(
        tmp.path(),
        vec![plan("r1", 1), plan("r2", 1)],
        vec![
            ApplyResult {
                written: vec![json!({"path":"a"})],
                rejected: vec![],
            },
            ApplyResult {
                written: vec![json!({"path":"a"})],
                rejected: vec![],
            },
        ],
        vec![
            VerifyResult {
                status: "fail".into(),
                findings: vec![
                    json!({"severity":"error","file":"docs/a.md","code":"X","message":"bad"}),
                ],
                retry_files: vec!["docs/a.md".into()],
            },
            VerifyResult {
                status: "pass".into(),
                ..Default::default()
            },
        ],
    );
    let mut dispatches = vec![];
    let out = Orchestrator::new(&cfg, &repo, &db, &run, true)
        .run_language_with("zh-CN", &skill, |stage, kind, _, _| {
            dispatches.push(format!("{stage}:{kind}"));
            Ok(vec![ok_task()])
        })
        .unwrap();
    assert_eq!(out.status, Status::Ok);
    assert_eq!(out.repair_rounds, 1);
    assert_eq!(
        skill.calls(),
        vec!["plan", "apply", "verify", "plan-repair", "apply", "verify"]
    );
    assert_eq!(dispatches, vec!["translate:tasks", "translate:tasks"]);
    assert!(out.transitions.contains(&"repairing:verify:1".into()));
    println!(
        "FLOW_AFTER verify_repair skill_calls={} agent_dispatches={} transitions={}",
        skill.calls().join("->"),
        dispatches.join("->"),
        out.transitions.join("->")
    );
}

#[test]
fn revision_is_called_only_when_enabled() {
    let tmp = tempdir().unwrap();
    let (cfg, repo, db, run) = setup(
        tmp.path(),
        RepoStages {
            revision: true,
            proofread: false,
        },
    );
    let mut skill = StubSkill::new(
        tmp.path(),
        vec![plan("r1", 1)],
        vec![ApplyResult {
            written: vec![],
            rejected: vec![],
        }],
        vec![VerifyResult {
            status: "pass".into(),
            ..Default::default()
        }],
    );
    skill
        .review_plans
        .get_mut()
        .unwrap()
        .push_back(SkillResult {
            returncode: 3,
            data: ReviewPlanResult::default(),
        });
    let out = Orchestrator::new(&cfg, &repo, &db, &run, true)
        .run_language_with("zh-CN", &skill, |_, _, _, _| Ok(vec![ok_task()]))
        .unwrap();
    assert_eq!(out.status, Status::Ok);
    assert!(skill.calls().contains(&"review-plan:revision".into()));
    assert!(!skill.calls().contains(&"review-plan:proofread".into()));
}

#[test]
fn proofread_dispatches_nonempty_review_and_collects_advisory_findings() {
    let tmp = tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("work/p1/review")).unwrap();
    fs::write(
        tmp.path().join("work/p1/review/docs-a-md.review.json"),
        "{}",
    )
    .unwrap();
    let (cfg, repo, db, run) = setup(
        tmp.path(),
        RepoStages {
            revision: false,
            proofread: true,
        },
    );
    let mut skill = StubSkill::new(
        tmp.path(),
        vec![plan("r1", 1)],
        vec![ApplyResult {
            written: vec![json!({"path":"docs/a.zh-CN.md"})],
            rejected: vec![],
        }],
        vec![VerifyResult {
            status: "pass".into(),
            ..Default::default()
        }],
    );
    skill
        .review_plans
        .get_mut()
        .unwrap()
        .push_back(SkillResult {
            returncode: 0,
            data: ReviewPlanResult {
                run_id: "p1".into(),
                task_count: 1,
            },
        });
    skill
        .reviews
        .get_mut()
        .unwrap()
        .push_back(ReviewCollectResult {
            findings: vec![json!({
                "severity": "error",
                "code": "STYLE",
                "message": "advisory wording"
            })],
        });

    let mut dispatches = vec![];
    let out = Orchestrator::new(&cfg, &repo, &db, &run, true)
        .run_language_with("zh-CN", &skill, |stage, kind, work, _| {
            dispatches.push(format!("{stage}:{kind}"));
            if kind == "review" {
                assert_eq!(work, tmp.path().join("work/p1"));
                assert_eq!(fani::skill::task_files(work, kind).len(), 1);
            }
            Ok(vec![ok_task()])
        })
        .unwrap();

    assert_eq!(out.status, Status::Ok);
    assert_eq!(dispatches, vec!["translate:tasks", "proofread:review"]);
    assert_eq!(
        skill.calls(),
        vec![
            "plan",
            "apply",
            "verify",
            "review-plan:proofread",
            "review-collect"
        ]
    );
    assert!(out.transitions.contains(&"reviewing:proofread".into()));
    assert!(out.transitions.contains(&"publishing".into()));
    assert!(out.transitions.contains(&"complete:ok".into()));
    assert_eq!(out.findings.len(), 1);
    assert_eq!(out.findings[0]["code"], "STYLE");
    println!(
        "FLOW_AFTER proofread skill_calls={} agent_dispatches={} transitions={}",
        skill.calls().join("->"),
        dispatches.join("->"),
        out.transitions.join("->")
    );
}
