import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani import adapters, dispatch
from fani.config import AgentConfig

FAKE = Path(__file__).resolve().parent / "fixtures" / "fake_agent.py"

SOURCE = "# Title\n\nSome prose.\n\n@@CODE_BLOCK_0@@\n\nMore prose."


def agent(mode: str, **kw) -> AgentConfig:
    return AgentConfig(
        name=f"fake-{mode}",
        cmd=(sys.executable, str(FAKE), mode),
        stages=("translate", "revision", "proofread"),
        concurrency=kw.pop("concurrency", 4),
        timeout_s=kw.pop("timeout_s", 30.0),
        retries=kw.pop("retries", 1),
        enabled=True,
    )


class PromptTest(unittest.TestCase):
    def test_translate_prompt_carries_prompt_and_source(self):
        text = dispatch.build_prompt({"prompt": "RULES HERE", "source": SOURCE})
        self.assertIn("RULES HERE", text)
        self.assertIn("@@CODE_BLOCK_0@@", text)
        self.assertIn("Output channel", text)

    def test_revise_prompt_carries_previous_translation(self):
        text = dispatch.build_prompt({
            "prompt": "RULES", "source": SOURCE, "mode": "revise",
            "previous_source": "old source", "previous_translation": "旧译文",
            "match_ratio": 0.91,
        })
        self.assertIn("旧译文", text)
        self.assertIn("old source", text)
        self.assertIn("0.91", text)
        self.assertIn("byte-identical", text)

    def test_repair_prompt_carries_findings(self):
        text = dispatch.build_prompt(
            {"prompt": "RULES", "source": SOURCE}, findings="X-INLINE expected=['`{count}`']"
        )
        self.assertIn("X-INLINE", text)
        self.assertIn("smallest change", text)

    def test_review_prompt_is_forwarded_verbatim(self):
        text = dispatch.build_review_prompt({"prompt": "ALREADY RENDERED"})
        self.assertTrue(text.startswith("ALREADY RENDERED"))
        self.assertIn("Output channel", text)


class NormaliseTest(unittest.TestCase):
    def test_strips_single_outer_fence(self):
        self.assertEqual(adapters.normalise("```markdown\nhi\n```"), "hi")
        self.assertEqual(adapters.normalise("```\nhi\n```"), "hi")

    def test_keeps_inner_fences(self):
        text = "# T\n\n```bash\nls\n```\n\nend"
        self.assertEqual(adapters.normalise(text), text)

    def test_keeps_content_starting_with_language_fence(self):
        text = "```bash\nls\n```"
        self.assertEqual(adapters.normalise(text), text)


class DispatcherTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        (self.root / "work" / "results").mkdir(parents=True)
        (self.root / "work" / "tasks").mkdir(parents=True)

    def make_task(self, task_id: str = "t1", **extra) -> Path:
        task = {
            "task_id": task_id,
            "chunk_id": "body:1",
            "prompt": "Translate into Chinese.",
            "source": SOURCE,
            "result_path": f"work/results/{task_id}.json",
        }
        task.update(extra)
        p = self.root / "work" / "tasks" / f"{task_id}.json"
        p.write_text(json.dumps(task), encoding="utf-8")
        return p

    def result(self, task_id: str = "t1") -> dict:
        return json.loads((self.root / "work" / "results" / f"{task_id}.json").read_text())

    def test_writes_result_file_the_skill_expects(self):
        task = self.make_task()
        out = dispatch.Dispatcher(self.root, agent("ok")).run([task])
        self.assertTrue(out[0].ok, out[0].message)
        data = self.result()
        self.assertEqual(data["chunk_id"], "body:1")
        self.assertIn("@@CODE_BLOCK_0@@", data["translated_text"])
        self.assertIn("[zh]", data["translated_text"])

    def test_agent_never_touches_the_result_file_itself(self):
        # The fake agent has no file access; the dispatcher is what writes.
        task = self.make_task()
        dispatch.Dispatcher(self.root, agent("ok")).run([task])
        self.assertTrue((self.root / "work" / "results" / "t1.json").is_file())

    def test_unwraps_json_envelope(self):
        task = self.make_task()
        out = dispatch.Dispatcher(self.root, agent("envelope")).run([task])
        self.assertTrue(out[0].ok)
        self.assertNotIn("translated_text", self.result()["translated_text"])
        self.assertIn("[zh]", self.result()["translated_text"])

    def test_strips_wrapping_fence(self):
        task = self.make_task()
        out = dispatch.Dispatcher(self.root, agent("fenced")).run([task])
        self.assertTrue(out[0].ok)
        self.assertFalse(self.result()["translated_text"].startswith("```markdown"))

    def test_empty_output_is_coded_and_retried(self):
        task = self.make_task()
        out = dispatch.Dispatcher(self.root, agent("empty", retries=1)).run([task])
        self.assertFalse(out[0].ok)
        self.assertEqual(out[0].code, "DSP-EMPTY")
        self.assertEqual(out[0].attempts, 2)
        self.assertFalse((self.root / "work" / "results" / "t1.json").exists())

    def test_non_zero_exit_is_coded(self):
        task = self.make_task()
        out = dispatch.Dispatcher(self.root, agent("fail", retries=0)).run([task])
        self.assertFalse(out[0].ok)
        self.assertEqual(out[0].code, "DSP-EXIT")

    def test_timeout_is_coded(self):
        task = self.make_task()
        out = dispatch.Dispatcher(self.root, agent("slow", timeout_s=1.0, retries=0)).run([task])
        self.assertFalse(out[0].ok)
        self.assertEqual(out[0].code, "DSP-TIMEOUT")

    def test_transient_failure_recovers_on_retry(self):
        import os

        counter = self.root / "counter"
        task = self.make_task()
        os.environ["FAKE_AGENT_COUNTER"] = str(counter)
        self.addCleanup(os.environ.pop, "FAKE_AGENT_COUNTER", None)
        out = dispatch.Dispatcher(self.root, agent("flaky", retries=2)).run([task])
        self.assertTrue(out[0].ok, out[0].message)
        self.assertEqual(out[0].attempts, 2)

    def test_review_requires_valid_json(self):
        task = self.make_task("r1")
        ok = dispatch.Dispatcher(self.root, agent("review")).run([task], kind="review")
        self.assertTrue(ok[0].ok, ok[0].message)
        self.assertEqual(self.result("r1"), {"findings": []})

        bad = self.make_task("r2")
        out = dispatch.Dispatcher(self.root, agent("ok", retries=0)).run([bad], kind="review")
        self.assertFalse(out[0].ok)
        self.assertEqual(out[0].code, "DSP-EMPTY")

    def test_runs_tasks_concurrently_and_logs_each(self):
        tasks = [self.make_task(f"t{i}") for i in range(5)]
        log = self.root / "dispatch.jsonl"
        out = dispatch.Dispatcher(self.root, agent("ok"), log_path=log).run(tasks)
        self.assertEqual(len(out), 5)
        self.assertTrue(all(o.ok for o in out))
        lines = [json.loads(x) for x in log.read_text().splitlines()]
        self.assertEqual(len(lines), 5)
        self.assertEqual({x["task_id"] for x in lines}, {f"t{i}" for i in range(5)})
        self.assertTrue(all("duration_s" in x for x in lines))

    def test_empty_task_list_is_a_no_op(self):
        self.assertEqual(dispatch.Dispatcher(self.root, agent("ok")).run([]), [])


if __name__ == "__main__":
    unittest.main()
