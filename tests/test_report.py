import json
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani.dispatch import TaskOutcome
from fani.orchestrator import LangOutcome, Status
from fani.report import build, exit_code, overall, render, write


def outcome(status: Status, **kw) -> LangOutcome:
    return LangOutcome(repo="/repos/docs", lang="zh-CN", status=status, **kw)


class OverallTest(unittest.TestCase):
    def test_empty_run_is_ok(self):
        self.assertIs(overall([]), Status.OK)
        self.assertEqual(exit_code([]), 0)

    def test_worst_status_wins(self):
        outs = [outcome(Status.OK), outcome(Status.PARTIAL), outcome(Status.NEEDS_HUMAN)]
        self.assertIs(overall(outs), Status.NEEDS_HUMAN)
        self.assertEqual(exit_code(outs), 1)

    def test_error_outranks_needs_human(self):
        outs = [outcome(Status.NEEDS_HUMAN), outcome(Status.ERROR)]
        self.assertEqual(exit_code(outs), 2)

    def test_partial_only_is_exit_three(self):
        self.assertEqual(exit_code([outcome(Status.OK), outcome(Status.PARTIAL)]), 3)


class BuildTest(unittest.TestCase):
    def test_totals_add_up(self):
        outs = [
            outcome(
                Status.OK,
                written=[{"path": "a.md"}, {"path": "b.md"}],
                fuzzy_matched=4,
                dispatch=[
                    TaskOutcome(task_id="t1", ok=True, code="", attempts=1,
                                duration_s=1.0, message=""),
                    TaskOutcome(task_id="t2", ok=False, code="DSP-TIMEOUT", attempts=3,
                                duration_s=2.0, message="timed out"),
                ],
            ),
            outcome(Status.PARTIAL, remaining_tasks=9, repair_rounds=2),
        ]
        data = build(outs, time.time(), Path("fani.toml"))
        self.assertEqual(data["status"], "partial")
        self.assertEqual(data["exit_code"], 3)
        self.assertEqual(data["totals"]["files_written"], 2)
        self.assertEqual(data["totals"]["agent_calls"], 2)
        self.assertEqual(data["totals"]["agent_failures"], 1)
        self.assertEqual(data["totals"]["remaining_tasks"], 9)
        self.assertEqual(data["totals"]["repair_rounds"], 2)
        self.assertEqual(data["totals"]["fuzzy_matched"], 4)


class RenderTest(unittest.TestCase):
    def test_clean_run_has_no_detail_sections(self):
        text = render(build([outcome(Status.OK, written=[{"path": "a.md"}])],
                            time.time(), Path("fani.toml")))
        self.assertIn("# fani run — ok", text)
        self.assertIn("| docs | zh-CN | ok |", text)
        self.assertNotIn("## docs", text)

    def test_problems_are_spelled_out(self):
        out = outcome(
            Status.NEEDS_HUMAN,
            message="2 translation(s) were edited by hand",
            conflicts=[{"path": "docs/a.zh-CN.md"}],
            findings=[{"file": "docs/a.md", "severity": "error", "code": "X-FENCE",
                       "message": "fence count differs"}],
            dispatch=[TaskOutcome(task_id="t2", ok=False, code="DSP-EXIT", attempts=3,
                                  duration_s=2.0, message="exit 3")],
        )
        text = render(build([out], time.time(), Path("fani.toml")))
        self.assertIn("# fani run — needs_human", text)
        self.assertIn("## docs — zh-CN", text)
        self.assertIn("docs/a.zh-CN.md", text)
        self.assertIn("X-FENCE", text)
        self.assertIn("DSP-EXIT", text)

    def test_long_finding_lists_are_truncated(self):
        findings = [{"file": f"f{i}.md", "severity": "error", "code": "X", "message": "m"}
                    for i in range(50)]
        text = render(build([outcome(Status.NEEDS_HUMAN, findings=findings)],
                            time.time(), Path("fani.toml")))
        self.assertIn("10 more (see report.json)", text)


class WriteTest(unittest.TestCase):
    def test_writes_both_artefacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "nested" / "reports"
            data = write([outcome(Status.OK)], out_dir, time.time(), Path("fani.toml"))
            self.assertEqual(
                json.loads((out_dir / "report.json").read_text())["status"], data["status"]
            )
            self.assertTrue((out_dir / "report.md").read_text().startswith("# fani run"))


if __name__ == "__main__":
    unittest.main()
