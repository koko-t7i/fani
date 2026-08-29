"""The state machine: plan, dispatch, apply, verify, repair, review, report.

Control flow lives here, never in a model. The agent is called only to turn one
chunk of source text into one chunk of translated text; every decision about
what to translate, whether it is acceptable, and whether to continue is made by
the skill's deterministic scripts and by the branches below.

Two guardrails exist because unattended runs cannot ask a question:

*Human edits stop the run.* ``plan`` reports a hand-edited translation as a
conflict. ``--force`` would discard someone's work, so fani never passes it.

*A suspiciously large batch stops the run.* A repository that already has
translations should not suddenly need dozens of fresh chunks. When it does, the
usual cause is a lost or reset ``state.json``, and continuing would re-translate
and re-bill the entire repository.
"""

import json
import re
import shutil
import time
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path

from .config import Config, RepoConfig
from .dispatch import Dispatcher, TaskOutcome
from .gitout import GitError, Published, publish
from .skill import Skill, SkillError, findings_for_tasks, task_files


class Status(StrEnum):
    OK = "ok"  # translated something, or nothing needed doing
    NEEDS_HUMAN = "needs_human"  # conflicts, exhausted budget, tripped guard
    PARTIAL = "partial"  # this batch is done, more chunks remain
    ERROR = "error"  # environment or skill failure


EXIT_CODES = {Status.OK: 0, Status.NEEDS_HUMAN: 1, Status.ERROR: 2, Status.PARTIAL: 3}


@dataclass
class LangOutcome:
    repo: str
    lang: str
    status: Status
    run_id: str = ""
    message: str = ""
    written: list[dict] = field(default_factory=list)
    conflicts: list[dict] = field(default_factory=list)
    findings: list[dict] = field(default_factory=list)
    dispatch: list[TaskOutcome] = field(default_factory=list)
    published: Published = field(default_factory=Published)
    repair_rounds: int = 0
    remaining_tasks: int = 0
    fuzzy_matched: int = 0
    duration_s: float = 0.0

    def to_dict(self) -> dict:
        return {
            "repo": self.repo,
            "lang": self.lang,
            "status": self.status.value,
            "run_id": self.run_id,
            "message": self.message,
            "written": self.written,
            "conflicts": self.conflicts,
            "findings": self.findings,
            "dispatch": [
                {
                    "task_id": d.task_id, "ok": d.ok, "code": d.code,
                    "attempts": d.attempts, "duration_s": round(d.duration_s, 3),
                    "message": d.message,
                }
                for d in self.dispatch
            ],
            "published": self.published.to_dict(),
            "repair_rounds": self.repair_rounds,
            "remaining_tasks": self.remaining_tasks,
            "fuzzy_matched": self.fuzzy_matched,
            "duration_s": round(self.duration_s, 3),
        }


def backup_state(skill: Skill, work: Path) -> Path | None:
    """Copy state.json aside before anything can rewrite it.

    The lockfile carries every source hash and the whole chunk cache. Losing it
    does not lose translations, but it makes the next run re-translate the
    repository from scratch, which is expensive and silent.
    """
    src = skill.state_path
    if not src.is_file():
        return None
    work.mkdir(parents=True, exist_ok=True)
    dest = work / "state.json.backup"
    shutil.copy2(src, dest)
    return dest


class Orchestrator:
    def __init__(self, cfg: Config, repo: RepoConfig, log=None):
        self.cfg = cfg
        self.repo = repo
        self.log = log or (lambda msg: None)
        self.skill = Skill(cfg.skill, repo.path, repo.state_dir)

    # -- guardrails ---------------------------------------------------------

    def _guard_tripped(self, plan: dict) -> str | None:
        """True when this looks like an accidental whole-repository retranslation."""
        limit = self.repo.full_retranslate_guard
        if limit <= 0 or plan.get("task_count", 0) <= limit:
            return None
        # A repository with no state at all is legitimately translating for the
        # first time; the guard is about losing state we used to have.
        if not self.skill.state_path.is_file():
            return None
        known = 0
        try:
            data = json.loads(self.skill.state_path.read_text(encoding="utf-8"))
            known = len(data.get("files", {}))
        except (OSError, ValueError):
            known = 0
        if known == 0:
            return None
        return (
            f"{plan['task_count']} fresh chunks exceeds full_retranslate_guard "
            f"({limit}) while state.json still records {known} file(s). This usually "
            f"means the chunk cache was lost or the chunker version changed. Review "
            f"the plan, then re-run with a raised guard if it is expected."
        )

    # -- stages -------------------------------------------------------------

    def _dispatch(self, work: Path, kind: str, findings: dict[str, str] | None,
                  stage: str) -> list[TaskOutcome]:
        tasks = task_files(work, kind)
        if not tasks:
            return []
        agent = self.cfg.agent_for(stage)
        self.log(f"    dispatching {len(tasks)} {kind} to {agent.name} "
                 f"(concurrency {agent.concurrency})")
        dispatcher = Dispatcher(self.repo.path, agent, log_path=work / "dispatch.jsonl")
        return dispatcher.run(tasks, kind=kind, findings_by_task=findings)

    def _apply_with_retry(self, run_id: str, work: Path) -> tuple[dict, list[TaskOutcome]]:
        """Apply, then re-dispatch the chunks of any rejected file exactly once.

        Rejections are the skill's own structural checks (ASM-*) catching a
        mangled placeholder or a missing chunk. Re-running those specific tasks
        is cheap; re-running the file is not.
        """
        extra: list[TaskOutcome] = []
        result = self.skill.apply(run_id)
        rejected = result.data.get("rejected", [])
        if rejected:
            names = {r["file"] for r in rejected}
            self.log(f"    apply rejected {len(rejected)} file(s); re-dispatching")
            redo = [
                t for t in task_files(work, "tasks")
                if any(t.stem.startswith(_slug(f)) for f in names)
            ]
            if redo:
                agent = self.cfg.agent_for("translate")
                dispatcher = Dispatcher(
                    self.repo.path, agent, log_path=work / "dispatch.jsonl"
                )
                extra = dispatcher.run(redo, kind="tasks")
                result = self.skill.apply(run_id)
        return result.data, extra

    # -- main ---------------------------------------------------------------

    def run_language(self, lang: str) -> LangOutcome:
        started = time.monotonic()
        out = LangOutcome(repo=str(self.repo.path), lang=lang, status=Status.OK)
        try:
            return self._run_language(lang, out, started)
        except SkillError as exc:
            out.status = Status.ERROR
            out.message = str(exc)
            out.duration_s = time.monotonic() - started
            return out

    def _run_language(self, lang: str, out: LangOutcome, started: float) -> LangOutcome:
        self.log(f"  {self.repo.path.name} [{lang}] planning")
        plan = self.skill.plan(
            lang, self.repo.paths, self.repo.exclude, self.repo.max_tasks
        ).data
        out.run_id = plan.get("run_id", "")
        out.fuzzy_matched = plan.get("fuzzy_matched", 0)
        out.remaining_tasks = plan.get("truncated_tasks", 0)
        work = self.skill.work_dir(out.run_id) if out.run_id else self.repo.path

        if plan.get("conflicts"):
            out.status = Status.NEEDS_HUMAN
            out.conflicts = plan["conflicts"]
            out.message = (
                f"{len(plan['conflicts'])} translation(s) were edited by hand; "
                f"fani will not overwrite them"
            )
            out.duration_s = time.monotonic() - started
            return out

        if not plan.get("task_count"):
            out.message = "every translation is up to date"
            out.duration_s = time.monotonic() - started
            self.log(f"  {self.repo.path.name} [{lang}] up to date")
            return out

        tripped = self._guard_tripped(plan)
        if tripped:
            out.status = Status.NEEDS_HUMAN
            out.message = tripped
            out.duration_s = time.monotonic() - started
            return out

        backup_state(self.skill, work)

        out.dispatch += self._dispatch(work, "tasks", None, "translate")
        applied, extra = self._apply_with_retry(out.run_id, work)
        out.dispatch += extra
        out.written = applied.get("written", [])
        if applied.get("rejected"):
            out.status = Status.NEEDS_HUMAN
            out.findings = applied["rejected"]
            out.message = (
                f"{len(applied['rejected'])} file(s) could not be assembled after a retry"
            )
            out.duration_s = time.monotonic() - started
            return out

        verify = self.skill.verify(lang).data
        while verify.get("status") == "fail" and out.repair_rounds < self.repo.repair_budget:
            out.repair_rounds += 1
            self.log(f"    verify failed; repair round {out.repair_rounds}")
            vpath = work / "verify.json"
            vpath.parent.mkdir(parents=True, exist_ok=True)
            vpath.write_text(json.dumps(verify, ensure_ascii=False), encoding="utf-8")
            rplan = self.skill.plan(
                lang, self.repo.paths, self.repo.exclude, self.repo.max_tasks, repair=vpath
            ).data
            if not rplan.get("task_count"):
                break
            rwork = self.skill.work_dir(rplan["run_id"])
            findings = findings_for_tasks(verify, task_files(rwork, "tasks"))
            out.dispatch += self._dispatch(rwork, "tasks", findings, "translate")
            applied, extra = self._apply_with_retry(rplan["run_id"], rwork)
            out.dispatch += extra
            out.written += applied.get("written", [])
            verify = self.skill.verify(lang).data

        out.findings = verify.get("findings", [])
        if verify.get("status") == "fail":
            out.status = Status.NEEDS_HUMAN
            out.message = (
                f"verification still failing after {out.repair_rounds} repair round(s): "
                f"{', '.join(verify.get('retry_files', [])) or 'see findings'}"
            )
            out.duration_s = time.monotonic() - started
            return out

        if self.repo.revision:
            self._revision(lang, out, work)
            if out.status is not Status.OK:
                out.duration_s = time.monotonic() - started
                return out

        # Only a run that got this far publishes. A run that needs a human leaves
        # its work in the tree, uncommitted, where the human will see it.
        try:
            out.published = publish(self.repo, lang, out.written)
            if out.published.commit:
                self.log(f"    committed {out.published.commit[:9]} on {out.published.branch}")
        except GitError as exc:
            out.status = Status.NEEDS_HUMAN
            out.message = f"translated, but could not commit: {exc}"
            out.duration_s = time.monotonic() - started
            return out

        if out.remaining_tasks:
            out.status = Status.PARTIAL
            out.message = (
                f"{out.remaining_tasks} chunk(s) exceeded max_tasks and were not planned; "
                f"the next run continues where this one stopped"
            )
        else:
            out.message = f"wrote {len(out.written)} file(s)"
        out.duration_s = time.monotonic() - started
        return out

    def _revision(self, lang: str, out: LangOutcome, work: Path) -> None:
        """Bilingual revision. Blocking findings send the file back through repair."""
        self.log(f"    revision pass for {lang}")
        plan = self.skill.review_plan(lang, "revision", run_id=out.run_id)
        if plan.returncode == 3 or not plan.data.get("task_count"):
            return
        rid = plan.data["run_id"]
        rwork = self.skill.work_dir(rid)
        out.dispatch += self._dispatch(rwork, "review", None, "revision")
        collected = self.skill.review_collect(rid)
        out.findings += collected.data.get("findings", [])
        blocking = [f for f in collected.data.get("findings", []) if f.get("severity") == "error"]
        if blocking:
            out.status = Status.NEEDS_HUMAN
            out.message = (
                f"revision found {len(blocking)} blocking issue(s) in "
                f"{len({f['file'] for f in blocking})} file(s)"
            )


def _slug(rel: str) -> str:
    return re.sub(r"[^A-Za-z0-9]+", "-", rel).strip("-").lower()
