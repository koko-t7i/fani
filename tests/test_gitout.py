import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "src"))

from fani.config import RepoConfig
from fani.gitout import Git, GitError, allowed_paths, publish

HAS_GIT = shutil.which("git") is not None


def git(root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True, check=True,
        env={"HOME": str(root), "PATH": "/usr/bin:/bin", "GIT_CONFIG_GLOBAL": "/dev/null",
             "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@e",
             "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@e"},
    ).stdout.strip()


@unittest.skipUnless(HAS_GIT, "git not installed")
class GitOutTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        git(self.root, "init", "-q", "-b", "main")
        git(self.root, "config", "user.email", "t@e")
        git(self.root, "config", "user.name", "t")
        (self.root / "docs").mkdir()
        (self.root / "docs" / "guide.md").write_text("source\n", encoding="utf-8")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-qm", "initial")
        self.repo = RepoConfig(path=self.root, languages=("zh-CN",), state_dir=".fani-state")
        self.git = Git(self.root)

    def translate(self, text="译文\n"):
        (self.root / "docs" / "guide.zh-CN.md").write_text(text, encoding="utf-8")
        state = self.root / ".fani-state"
        state.mkdir(exist_ok=True)
        (state / "state.json").write_text('{"files": {}}', encoding="utf-8")
        return [{"path": "docs/guide.zh-CN.md"}]

    def head_of(self, ref: str) -> str:
        return git(self.root, "rev-parse", ref)

    def files_in(self, ref: str) -> set[str]:
        return set(git(self.root, "ls-tree", "-r", "--name-only", ref).splitlines())

    def test_commit_lands_on_the_language_branch_without_moving_head(self):
        written = self.translate()
        before = self.head_of("HEAD")
        result = publish(self.repo, "zh-CN", written, self.git)

        self.assertEqual(result.branch, "i18n/zh-CN")
        self.assertTrue(result.commit)
        self.assertEqual(self.head_of("HEAD"), before)
        self.assertEqual(git(self.root, "rev-parse", "--abbrev-ref", "HEAD"), "main")
        self.assertIn("docs/guide.zh-CN.md", self.files_in("i18n/zh-CN"))

    def test_translation_and_state_share_one_commit(self):
        publish(self.repo, "zh-CN", self.translate(), self.git)
        names = self.files_in("i18n/zh-CN")
        self.assertIn("docs/guide.zh-CN.md", names)
        self.assertIn(".fani-state/state.json", names)

    def test_unrelated_working_tree_changes_are_never_committed(self):
        written = self.translate()
        (self.root / "docs" / "guide.md").write_text("edited by a person\n", encoding="utf-8")
        (self.root / "scratch.txt").write_text("mine\n", encoding="utf-8")
        publish(self.repo, "zh-CN", written, self.git)

        committed = git(self.root, "show", "--stat", "--name-only", "--format=", "i18n/zh-CN")
        self.assertNotIn("scratch.txt", committed)
        self.assertNotIn("docs/guide.md\n", committed + "\n")
        self.assertEqual(
            git(self.root, "show", "i18n/zh-CN:docs/guide.md"), "source"
        )

    def test_a_second_run_stacks_onto_the_same_branch(self):
        publish(self.repo, "zh-CN", self.translate(), self.git)
        first = self.head_of("i18n/zh-CN")
        second = publish(self.repo, "zh-CN", self.translate("修订后的译文\n"), self.git)
        self.assertNotEqual(second.commit, first)
        self.assertEqual(self.head_of("i18n/zh-CN^"), first)

    def test_nothing_changed_means_nothing_committed(self):
        written = self.translate()
        publish(self.repo, "zh-CN", written, self.git)
        again = publish(self.repo, "zh-CN", written, self.git)
        self.assertEqual(again.commit, "")
        self.assertIn("already committed", again.skipped)

    def test_commit_disabled_is_a_no_op(self):
        repo = RepoConfig(path=self.root, languages=("zh-CN",), state_dir=".fani-state",
                          commit=False)
        result = publish(repo, "zh-CN", self.translate(), self.git)
        self.assertEqual(result.commit, "")
        self.assertIn("disabled", result.skipped)
        self.assertFalse(self.git.rev("i18n/zh-CN"))

    def test_empty_run_commits_nothing(self):
        result = publish(self.repo, "zh-CN", [], self.git)
        self.assertIn("nothing was written", result.skipped)

    def test_push_sends_the_branch_to_the_remote(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        remote = Path(holder.name) / "remote.git"
        subprocess.run(["git", "init", "-q", "--bare", str(remote)], check=True)
        git(self.root, "remote", "add", "origin", str(remote))
        repo = RepoConfig(path=self.root, languages=("zh-CN",), state_dir=".fani-state",
                          push=True)
        result = publish(repo, "zh-CN", self.translate(), self.git)
        self.assertTrue(result.pushed)
        pushed = subprocess.run(
            ["git", "rev-parse", "i18n/zh-CN"], cwd=remote, capture_output=True, text=True,
        ).stdout.strip()
        self.assertEqual(pushed, result.commit)

    def test_apply_records_are_read_by_their_target_key(self):
        self.translate()
        record = [{"file": "docs/guide.md", "target": "docs/guide.zh-CN.md", "fresh": 1}]
        self.assertIn("docs/guide.zh-CN.md", allowed_paths(self.repo, record))

    def test_run_scratch_is_never_committed(self):
        written = self.translate()
        work = self.root / ".fani-state" / "work" / "run1" / "tasks"
        work.mkdir(parents=True)
        (work / "t1.json").write_text("{}", encoding="utf-8")
        (self.root / ".fani-state" / "fani.lock").write_text("{}", encoding="utf-8")
        publish(self.repo, "zh-CN", written, self.git)
        names = self.files_in("i18n/zh-CN")
        self.assertNotIn(".fani-state/fani.lock", names)
        self.assertFalse([n for n in names if n.startswith(".fani-state/work/")])

    def test_a_path_escaping_the_repository_is_refused(self):
        with self.assertRaises(GitError) as ctx:
            allowed_paths(self.repo, [{"path": "../outside.md"}])
        self.assertIn("outside the repository", str(ctx.exception))

    def test_publishing_outside_a_repository_explains_itself(self):
        with tempfile.TemporaryDirectory() as plain:
            repo = RepoConfig(path=Path(plain), languages=("zh-CN",))
            with self.assertRaises(GitError) as ctx:
                publish(repo, "zh-CN", [{"path": "a.md"}], Git(Path(plain)))
            self.assertIn("not a git repository", str(ctx.exception))

    def test_branch_pattern_is_configurable(self):
        repo = RepoConfig(path=self.root, languages=("zh-CN",), state_dir=".fani-state",
                          branch="translations/{lang}")
        result = publish(repo, "zh-CN", self.translate(), self.git)
        self.assertEqual(result.branch, "translations/zh-CN")
        self.assertTrue(self.git.rev("translations/zh-CN"))


if __name__ == "__main__":
    unittest.main()
