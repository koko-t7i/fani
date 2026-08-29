import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani import config

MINIMAL = """
skill = "{skill}"

[[repo]]
path = "{repo}"
languages = ["zh-CN"]

[agents.fake]
cmd = ["true"]
stages = ["translate"]

[routing]
translate = "fake"
"""


class ExampleConfigTest(unittest.TestCase):
    """The shipped example is the documentation; it has to stay loadable."""

    def test_example_config_parses(self):
        path = Path(__file__).resolve().parent.parent / "examples" / "fani.toml"
        cfg = config.load(path)
        self.assertEqual(len(cfg.repos), 2)
        self.assertEqual(cfg.agent_for("translate").name, "claude")
        self.assertTrue(str(cfg.skill).startswith("/"))  # ~ was expanded
        self.assertFalse(cfg.repos[1].commit)


class ConfigTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        (self.root / "skill" / "scripts").mkdir(parents=True)
        (self.root / "skill" / "scripts" / "run.sh").write_text("#!/bin/sh\n")
        (self.root / "repo").mkdir()

    def write(self, text: str) -> Path:
        p = self.root / "fani.toml"
        p.write_text(text, encoding="utf-8")
        return p

    def minimal(self) -> Path:
        return self.write(MINIMAL.format(
            skill=self.root / "skill", repo=self.root / "repo",
        ))

    def test_loads_and_applies_defaults(self):
        cfg = config.load(self.minimal())
        self.assertEqual(cfg.repos[0].languages, ("zh-CN",))
        self.assertEqual(cfg.repos[0].max_tasks, 40)
        self.assertEqual(cfg.repos[0].repair_budget, 2)
        self.assertFalse(cfg.repos[0].revision)
        self.assertEqual(cfg.agent_for("translate").name, "fake")
        config.check_environment(cfg)

    def test_missing_file_fails_loudly(self):
        with self.assertRaises(config.ConfigError):
            config.load(self.root / "nope.toml")

    def test_malformed_toml_fails_loudly(self):
        with self.assertRaises(config.ConfigError):
            config.load(self.write("skill = "))

    def test_repo_required(self):
        with self.assertRaises(config.ConfigError) as ctx:
            config.load(self.write('skill = "x"\n[agents.a]\ncmd = ["true"]\n'))
        self.assertIn("[[repo]]", str(ctx.exception))

    def test_routing_to_unknown_agent_fails(self):
        text = MINIMAL.format(skill=self.root / "skill", repo=self.root / "repo")
        with self.assertRaises(config.ConfigError) as ctx:
            config.load(self.write(text.replace('translate = "fake"', 'translate = "ghost"')))
        self.assertIn("ghost", str(ctx.exception))

    def test_routing_to_disabled_agent_fails(self):
        text = MINIMAL.format(skill=self.root / "skill", repo=self.root / "repo")
        text = text.replace('stages = ["translate"]', 'stages = ["translate"]\nenabled = false')
        with self.assertRaises(config.ConfigError) as ctx:
            config.load(self.write(text))
        self.assertIn("disabled", str(ctx.exception))

    def test_unknown_stage_rejected(self):
        text = MINIMAL.format(skill=self.root / "skill", repo=self.root / "repo")
        with self.assertRaises(config.ConfigError):
            config.load(self.write(text.replace('["translate"]', '["translating"]')))

    def test_agent_for_falls_back_when_routing_absent(self):
        text = MINIMAL.format(skill=self.root / "skill", repo=self.root / "repo")
        cfg = config.load(self.write(text.replace('[routing]\ntranslate = "fake"', "")))
        self.assertEqual(cfg.agent_for("translate").name, "fake")
        with self.assertRaises(config.ConfigError):
            cfg.agent_for("revision")

    def test_check_environment_reports_missing_skill(self):
        text = MINIMAL.format(skill=self.root / "absent", repo=self.root / "repo")
        cfg = config.load(self.write(text))
        with self.assertRaises(config.ConfigError) as ctx:
            config.check_environment(cfg)
        self.assertIn("run.sh", str(ctx.exception))

    def test_check_environment_reports_missing_repo(self):
        text = MINIMAL.format(skill=self.root / "skill", repo=self.root / "absent")
        cfg = config.load(self.write(text))
        with self.assertRaises(config.ConfigError) as ctx:
            config.check_environment(cfg)
        self.assertIn("repo path", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
