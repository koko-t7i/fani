"""End-to-end runs against the real i18n skill with a scripted stand-in agent.

These exercise the parts no unit test can: the skill's own JSON contracts, the
prompts it writes, and whether an answer produced by something other than a
model survives ``apply`` and ``verify``. They are skipped when the skill or uv
is not installed, so a checkout without them still has a green suite.

Point ``FANI_TEST_SKILL`` at an i18n skill directory to test another copy.
"""

import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani.cli import main

FIXTURES = Path(__file__).resolve().parent / "fixtures"
CANDIDATES = [
    Path(p) for p in [
        os.environ.get("FANI_TEST_SKILL", ""),
        Path.home() / ".claude/skills/i18n",
        Path(__file__).resolve().parents[2] / "skills/i18n/i18n",
    ] if p
]
SKILL = next((p for p in CANDIDATES if (p / "scripts" / "run.sh").is_file()), None)

SOURCE = """# Getting started

This is a short guide for new users.

```bash
echo hello
```

Run the command above, then read the `config.toml` file.

## Next steps

Nothing else to do.
"""

CONFIG = """
skill = "{skill}"

[[repo]]
path = "{repo}"
languages = ["zh-CN"]
state_dir = ".fani-state"
max_tasks = {max_tasks}
repair_budget = {repair_budget}

[agents.fake]
cmd = ["{python}", "{agent}", "{mode}"]
stages = ["translate", "revision"]
concurrency = 2
timeout_s = {timeout}
retries = {retries}

[routing]
translate = "fake"
revision = "fake"
"""


@unittest.skipIf(SKILL is None, "i18n skill not installed")
@unittest.skipIf(shutil.which("uv") is None, "uv not on PATH")
class EndToEndTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        self.repo = base / "repo"
        (self.repo / "docs").mkdir(parents=True)
        self.source = self.repo / "docs" / "guide.md"
        self.source.write_text(SOURCE, encoding="utf-8")
        self.target = self.repo / "docs" / "guide.zh-CN.md"
        # Reports live outside the repository; otherwise the next run would try
        # to translate them.
        self.reports = base / "reports"
        self.config = base / "fani.toml"

    def write_config(self, mode="ok", *, max_tasks=5, repair_budget=1, retries=0,
                     timeout=60, python=None):
        self.config.write_text(
            CONFIG.format(
                skill=SKILL, repo=self.repo, agent=FIXTURES / "fake_agent.py",
                mode=mode, max_tasks=max_tasks, repair_budget=repair_budget,
                retries=retries, timeout=timeout, python=python or sys.executable,
            ),
            encoding="utf-8",
        )

    def run_cli(self, *argv: str) -> int:
        with contextlib.redirect_stdout(io.StringIO()):
            return main(list(argv))

    def sync(self) -> int:
        return self.run_cli("sync", "--config", str(self.config), "--report-dir",
                            str(self.reports), "--quiet")

    def report(self) -> dict:
        return json.loads((self.reports / "report.json").read_text(encoding="utf-8"))

    def test_translation_is_written_and_the_second_run_is_a_no_op(self):
        self.write_config()
        self.assertEqual(self.sync(), 0)

        text = self.target.read_text(encoding="utf-8")
        self.assertIn("[zh] ", text)
        self.assertIn("echo hello", text)  # the code block survived untouched
        self.assertIn("`config.toml`", text)  # inline code survived
        first = self.report()
        self.assertEqual(first["totals"]["files_written"], 1)
        self.assertEqual(first["totals"]["agent_failures"], 0)

        self.assertEqual(self.sync(), 0)
        second = self.report()
        self.assertEqual(second["status"], "ok")
        self.assertEqual(second["totals"]["agent_calls"], 0)
        self.assertEqual(second["languages"][0]["message"], "every translation is up to date")

    def test_edited_translation_is_never_overwritten(self):
        self.write_config()
        self.assertEqual(self.sync(), 0)
        self.target.write_text(
            self.target.read_text(encoding="utf-8") + "\n人工补充的一段。\n", encoding="utf-8"
        )
        kept = self.target.read_text(encoding="utf-8")

        self.assertEqual(self.sync(), 1)
        self.assertEqual(self.target.read_text(encoding="utf-8"), kept)
        report = self.report()
        self.assertEqual(report["status"], "needs_human")
        self.assertTrue(report["languages"][0]["conflicts"])

    def test_a_failing_agent_leaves_the_repository_untouched(self):
        self.write_config(mode="fail")
        self.assertEqual(self.sync(), 1)
        self.assertFalse(self.target.exists())
        report = self.report()
        self.assertEqual(report["status"], "needs_human")
        self.assertGreater(report["totals"]["agent_failures"], 0)
        self.assertEqual(
            {d["code"] for d in report["languages"][0]["dispatch"] if not d["ok"]}, {"DSP-EXIT"}
        )

    def test_mangled_placeholders_are_rejected_not_written(self):
        self.write_config(mode="mangle")
        self.assertEqual(self.sync(), 1)
        self.assertFalse(self.target.exists())
        report = self.report()
        self.assertEqual(report["status"], "needs_human")
        self.assertIn("could not be assembled", report["languages"][0]["message"])

    def test_a_fenced_answer_is_unwrapped(self):
        self.write_config(mode="fenced")
        self.assertEqual(self.sync(), 0)
        text = self.target.read_text(encoding="utf-8")
        self.assertFalse(text.lstrip().startswith("```markdown"))
        self.assertIn("[zh] ", text)

    def test_a_json_envelope_answer_is_unwrapped(self):
        self.write_config(mode="envelope")
        self.assertEqual(self.sync(), 0)
        text = self.target.read_text(encoding="utf-8")
        self.assertNotIn("translated_text", text)
        self.assertIn("[zh] ", text)

    def test_a_timeout_is_reported_and_writes_nothing(self):
        self.write_config(mode="slow", timeout=1)
        self.assertEqual(self.sync(), 1)
        self.assertFalse(self.target.exists())
        codes = {d["code"] for d in self.report()["languages"][0]["dispatch"] if not d["ok"]}
        self.assertEqual(codes, {"DSP-TIMEOUT"})

    def test_a_concurrent_run_is_refused(self):
        self.write_config()
        lock = self.repo / ".fani-state" / "fani.lock"
        lock.parent.mkdir(parents=True, exist_ok=True)
        lock.write_text(json.dumps({"pid": os.getpid(), "started": time.time()}))
        self.assertEqual(self.sync(), 2)
        self.assertFalse(self.target.exists())
        self.assertIn("another run holds", self.report()["languages"][0]["message"])

    def test_status_plans_without_calling_an_agent(self):
        self.write_config(mode="fail")
        self.assertEqual(self.run_cli("status", "--config", str(self.config)), 0)
        self.assertFalse(self.target.exists())

    def test_doctor_fails_on_a_missing_agent_binary(self):
        self.write_config(python="/nonexistent/python")
        self.assertEqual(self.run_cli("doctor", "--config", str(self.config)), 2)

    def test_doctor_passes_on_a_healthy_setup(self):
        self.write_config()
        self.assertEqual(self.run_cli("doctor", "--config", str(self.config)), 0)

    def test_unknown_repo_or_language_is_a_config_error(self):
        self.write_config()
        self.assertEqual(self.run_cli("sync", "--config", str(self.config), "--repo", "nope"), 2)
        self.assertEqual(self.run_cli("sync", "--config", str(self.config), "--lang", "fr"), 2)

    def test_missing_config_is_exit_two(self):
        self.assertEqual(self.run_cli("sync", "--config", str(self.config.parent / "no.toml")), 2)


if __name__ == "__main__":
    unittest.main()
