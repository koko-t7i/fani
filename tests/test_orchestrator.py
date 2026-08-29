import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani.config import AgentConfig, Config, RepoConfig
from fani.orchestrator import EXIT_CODES, Orchestrator, Status, backup_state
from fani.skill import SkillError, SkillResult, findings_for_tasks

AGENT = AgentConfig(name="fake", cmd=("true",), stages=("translate", "revision"))


def plan_result(**kw) -> SkillResult:
    data = {"run_id": "r1", "task_count": 0, "conflicts": [], "files": [],
            "fuzzy_matched": 0, "truncated_tasks": 0}
    data.update(kw)
    return SkillResult(0, data, "", "")


class StubSkill:
    """Replays scripted skill responses and records the calls made."""

    def __init__(self, root: Path, plans=(), applies=(), verifies=(),
                 review_plan=None, review_collect=None):
        self.root = root
        self.state_path = root / "state.json"
        self.plans = list(plans) or [plan_result()]
        self.applies = list(applies) or [SkillResult(0, {"written": [], "rejected": []}, "", "")]
        self.verifies = list(verifies) or [
            SkillResult(0, {"status": "pass", "findings": []}, "", "")
        ]
        self._review_plan = review_plan
        self._review_collect = review_collect
        self.calls: list[str] = []

    def _next(self, queue, name):
        self.calls.append(name)
        return queue.pop(0) if len(queue) > 1 else queue[0]

    def work_dir(self, run_id):
        return self.root / "work" / run_id

    def plan(self, lang, paths=(), exclude=(), max_tasks=40, repair=None):
        return self._next(self.plans, "plan-repair" if repair else "plan")

    def apply(self, run_id):
        return self._next(self.applies, "apply")

    def verify(self, lang):
        return self._next(self.verifies, "verify")

    def review_plan(self, lang, mode, run_id=None):
        self.calls.append("review-plan")
        return self._review_plan or SkillResult(3, {}, "", "")

    def review_collect(self, run_id):
        self.calls.append("review-collect")
        return self._review_collect or SkillResult(0, {"findings": []}, "", "")


class OrchestratorTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        self.repo = RepoConfig(path=self.root, languages=("zh-CN",), state_dir=".")
        self.cfg = Config(skill=self.root, repos=(self.repo,), agents={"fake": AGENT},
                          routing={"translate": "fake", "revision": "fake"})

    def build(self, skill, **repo_kw) -> Orchestrator:
        if repo_kw:
            self.repo = RepoConfig(path=self.root, languages=("zh-CN",), state_dir=".", **repo_kw)
        orch = Orchestrator(self.cfg, self.repo)
        orch.skill = skill
        orch._dispatch = lambda work, kind, findings, stage: []
        return orch

    def write_state(self, files: int = 5):
        self.root.joinpath("state.json").write_text(
            json.dumps({"files": {f"doc{i}.md": {} for i in range(files)}}), encoding="utf-8"
        )

    def test_nothing_to_do_is_ok(self):
        orch = self.build(StubSkill(self.root))
        out = orch.run_language("zh-CN")
        self.assertIs(out.status, Status.OK)
        self.assertEqual(out.message, "every translation is up to date")
        self.assertEqual(EXIT_CODES[out.status], 0)

    def test_hand_edited_translation_stops_the_run(self):
        skill = StubSkill(
            self.root, plans=[plan_result(task_count=2, conflicts=[{"path": "a.md"}])]
        )
        out = self.build(skill).run_language("zh-CN")
        self.assertIs(out.status, Status.NEEDS_HUMAN)
        self.assertEqual(out.conflicts, [{"path": "a.md"}])
        self.assertNotIn("apply", skill.calls)

    def test_happy_path_writes_files(self):
        skill = StubSkill(
            self.root,
            plans=[plan_result(task_count=3)],
            applies=[SkillResult(0, {"written": [{"path": "zh/a.md"}], "rejected": []}, "", "")],
        )
        out = self.build(skill).run_language("zh-CN")
        self.assertIs(out.status, Status.OK)
        self.assertEqual(len(out.written), 1)
        self.assertEqual(out.message, "wrote 1 file(s)")

    def test_truncated_plan_reports_partial(self):
        skill = StubSkill(self.root, plans=[plan_result(task_count=3, truncated_tasks=7)])
        out = self.build(skill).run_language("zh-CN")
        self.assertIs(out.status, Status.PARTIAL)
        self.assertEqual(out.remaining_tasks, 7)
        self.assertEqual(EXIT_CODES[out.status], 3)

    def test_guard_trips_when_state_exists_and_batch_is_huge(self):
        self.write_state()
        skill = StubSkill(self.root, plans=[plan_result(task_count=99)])
        out = self.build(skill, full_retranslate_guard=10).run_language("zh-CN")
        self.assertIs(out.status, Status.NEEDS_HUMAN)
        self.assertIn("full_retranslate_guard", out.message)
        self.assertNotIn("apply", skill.calls)

    def test_guard_does_not_trip_on_a_first_translation(self):
        skill = StubSkill(self.root, plans=[plan_result(task_count=99)])
        out = self.build(skill, full_retranslate_guard=10).run_language("zh-CN")
        self.assertIs(out.status, Status.OK)

    def test_guard_can_be_disabled(self):
        self.write_state()
        skill = StubSkill(self.root, plans=[plan_result(task_count=99)])
        out = self.build(skill, full_retranslate_guard=0).run_language("zh-CN")
        self.assertIs(out.status, Status.OK)

    def test_rejection_surviving_a_retry_needs_human(self):
        rejected = SkillResult(
            0, {"written": [], "rejected": [{"file": "a.md", "code": "ASM-MISSING"}]}, "", ""
        )
        skill = StubSkill(self.root, plans=[plan_result(task_count=1)], applies=[rejected])
        out = self.build(skill).run_language("zh-CN")
        self.assertIs(out.status, Status.NEEDS_HUMAN)
        self.assertIn("could not be assembled", out.message)

    def test_repair_round_recovers(self):
        skill = StubSkill(
            self.root,
            plans=[plan_result(task_count=2), plan_result(run_id="r2", task_count=1)],
            applies=[SkillResult(0, {"written": [{"path": "zh/a.md"}], "rejected": []}, "", "")],
            verifies=[
                SkillResult(1, {"status": "fail", "findings": [
                    {"severity": "error", "file": "a.md", "code": "X-CODE", "message": "bad"},
                ]}, "", ""),
                SkillResult(0, {"status": "pass", "findings": []}, "", ""),
            ],
        )
        out = self.build(skill).run_language("zh-CN")
        self.assertIs(out.status, Status.OK)
        self.assertEqual(out.repair_rounds, 1)
        self.assertIn("plan-repair", skill.calls)

    def test_repair_budget_is_finite(self):
        failing = SkillResult(
            1, {"status": "fail", "findings": [], "retry_files": ["a.md"]}, "", ""
        )
        skill = StubSkill(
            self.root,
            plans=[plan_result(task_count=2), plan_result(run_id="r2", task_count=1)],
            verifies=[failing],
        )
        out = self.build(skill, repair_budget=2).run_language("zh-CN")
        self.assertIs(out.status, Status.NEEDS_HUMAN)
        self.assertEqual(out.repair_rounds, 2)
        self.assertIn("a.md", out.message)

    def test_repair_stops_when_the_repair_plan_is_empty(self):
        failing = SkillResult(1, {"status": "fail", "findings": [], "retry_files": []}, "", "")
        skill = StubSkill(
            self.root,
            plans=[plan_result(task_count=2), plan_result(run_id="r2", task_count=0)],
            verifies=[failing],
        )
        out = self.build(skill, repair_budget=5).run_language("zh-CN")
        self.assertIs(out.status, Status.NEEDS_HUMAN)
        self.assertEqual(out.repair_rounds, 1)

    def test_revision_blocking_findings_need_human(self):
        skill = StubSkill(
            self.root,
            plans=[plan_result(task_count=1)],
            review_plan=SkillResult(0, {"run_id": "rv1", "task_count": 1}, "", ""),
            review_collect=SkillResult(0, {"findings": [
                {"severity": "error", "file": "a.md", "code": "MQM-ACC", "message": "meaning lost"}
            ]}, "", ""),
        )
        out = self.build(skill, revision=True).run_language("zh-CN")
        self.assertIs(out.status, Status.NEEDS_HUMAN)
        self.assertIn("1 blocking issue", out.message)

    def test_revision_without_blocking_findings_stays_ok(self):
        skill = StubSkill(
            self.root,
            plans=[plan_result(task_count=1)],
            review_plan=SkillResult(0, {"run_id": "rv1", "task_count": 1}, "", ""),
            review_collect=SkillResult(
                0, {"findings": [{"severity": "warning", "file": "a.md"}]}, "", ""
            ),
        )
        out = self.build(skill, revision=True).run_language("zh-CN")
        self.assertIs(out.status, Status.OK)
        self.assertEqual(len(out.findings), 1)

    def test_revision_is_skipped_when_not_configured(self):
        skill = StubSkill(self.root, plans=[plan_result(task_count=1)])
        self.build(skill).run_language("zh-CN")
        self.assertNotIn("review-plan", skill.calls)

    def test_skill_failure_becomes_error(self):
        class Boom(StubSkill):
            def plan(self, *a, **kw):
                raise SkillError("run.sh not executable")

        out = self.build(Boom(self.root)).run_language("zh-CN")
        self.assertIs(out.status, Status.ERROR)
        self.assertEqual(EXIT_CODES[out.status], 2)
        self.assertIn("run.sh", out.message)


class BackupStateTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)

    def test_copies_state_when_present(self):
        skill = StubSkill(self.root)
        self.root.joinpath("state.json").write_text('{"files": {}}', encoding="utf-8")
        dest = backup_state(skill, self.root / "work")
        self.assertEqual(dest.read_text(), '{"files": {}}')

    def test_returns_none_without_state(self):
        self.assertIsNone(backup_state(StubSkill(self.root), self.root / "work"))


class FindingsMappingTest(unittest.TestCase):
    def test_findings_reach_the_tasks_of_their_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            a = work / "docs-guide-md-zh-cn.body-1.json"
            b = work / "docs-other-md-zh-cn.body-1.json"
            a.touch()
            b.touch()
            verify = {"findings": [
                {"severity": "error", "file": "docs/guide.md", "code": "X-FENCE",
                 "message": "fence count differs", "expected": 2, "actual": 1},
                {"severity": "warning", "file": "docs/other.md", "code": "X-LEN",
                 "message": "ignored"},
            ]}
            mapped = findings_for_tasks(verify, [a, b])
            self.assertIn(a.stem, mapped)
            self.assertNotIn(b.stem, mapped)
            self.assertIn("X-FENCE", mapped[a.stem])
            self.assertIn("expected=2", mapped[a.stem])


if __name__ == "__main__":
    unittest.main()
