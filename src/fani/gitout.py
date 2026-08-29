"""Publishing a run as a git commit on a branch of its own.

fani commits with plumbing (``read-tree`` / ``write-tree`` / ``commit-tree``)
into a temporary index, so it never touches HEAD, the real index, or a single
file in the working tree. A scheduled job may run in a checkout a person is
also using; switching branches under them would be unacceptable, and staging
their unrelated work into a translation commit would be worse.

The branch is fixed per language (``i18n/{lang}`` by default) and moves forward
across runs, so a truncated run continues onto the same branch and the reviewer
sees one pull request per language rather than one per run.

Translations and ``state.json`` land in the same commit. Splitting them lets a
merge take the text while dropping the record of what produced it, and the next
run would then translate everything again.
"""

import os
import subprocess
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

from .config import RepoConfig


class GitError(RuntimeError):
    """A git command failed, or the repository is not in a publishable state."""


@dataclass
class Published:
    branch: str = ""
    commit: str = ""
    paths: list[str] = field(default_factory=list)
    pushed: bool = False
    skipped: str = ""  # why nothing was committed, when nothing was

    def to_dict(self) -> dict:
        return {
            "branch": self.branch, "commit": self.commit, "paths": self.paths,
            "pushed": self.pushed, "skipped": self.skipped,
        }


class Git:
    def __init__(self, root: Path, timeout_s: float = 120.0):
        self.root = Path(root)
        self.timeout_s = timeout_s

    def run(self, *args: str, env: dict | None = None, check: bool = True) -> str:
        try:
            proc = subprocess.run(
                ["git", *args], cwd=str(self.root), capture_output=True, text=True,
                timeout=self.timeout_s, env={**os.environ, **(env or {})},
            )
        except subprocess.TimeoutExpired as exc:
            raise GitError(f"git {args[0]} timed out after {self.timeout_s:.0f}s") from exc
        except OSError as exc:
            raise GitError(f"cannot run git: {exc}") from exc
        if check and proc.returncode != 0:
            raise GitError(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
        return proc.stdout.strip()

    def is_repo(self) -> bool:
        return bool(self.run("rev-parse", "--is-inside-work-tree", check=False))

    def rev(self, ref: str) -> str:
        return self.run("rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}", check=False)

    def commit(self, paths: list[str], branch: str, message: str) -> str:
        """Commit ``paths`` from the working tree onto ``branch``; return the new sha.

        Returns ``""`` when the result would be identical to the branch tip,
        because a run that changed nothing should not leave an empty commit
        behind on every schedule tick.

        The parent is the branch tip when it exists, otherwise HEAD, so reruns
        stack onto the same branch instead of forking a new one each time.
        """
        tip = self.rev(branch)
        base = tip or self.rev("HEAD")
        with tempfile.TemporaryDirectory() as tmp:
            env = {"GIT_INDEX_FILE": str(Path(tmp) / "index")}
            if base:
                self.run("read-tree", base, env=env)
            self.run("update-index", "--add", "--", *paths, env=env)
            tree = self.run("write-tree", env=env)
        if tip and tree == self.run("rev-parse", f"{tip}^{{tree}}"):
            return ""
        parents = ["-p", base] if base else []
        sha = self.run("commit-tree", tree, *parents, "-m", message)
        # The old value guards against a branch that moved while this run worked:
        # forty zeros mean "this ref must not exist yet".
        self.run("update-ref", f"refs/heads/{branch}", sha, tip or "0" * 40)
        return sha

    def push(self, remote: str, branch: str) -> None:
        self.run("push", remote, f"refs/heads/{branch}:refs/heads/{branch}")


#: Files in the state directory worth keeping in version control. The ``work/``
#: subtree (per-run tasks and prompts) and the lock file are run scratch.
STATE_FILES = ("state.json", "glossary.json", "style.json")


def allowed_paths(repo: RepoConfig, written: list[dict]) -> list[str]:
    """Repository-relative paths this run is permitted to commit.

    Only the files ``apply`` reported writing, plus the translation memory and
    its glossary. Everything else in the tree belongs to a person, and a
    scheduled job has no business committing it.
    """
    root = repo.path.resolve()
    out = []
    for item in written:
        # apply records the translated file under "target"; "path" is accepted
        # so callers can pass a plain list of paths.
        rel = (item.get("target") or item.get("path")) if isinstance(item, dict) else item
        if not rel:
            continue
        target = (root / rel).resolve()
        if not target.is_relative_to(root):
            raise GitError(f"apply reported a path outside the repository: {rel}")
        out.append(str(target.relative_to(root)))
    for name in STATE_FILES:
        rel = f"{repo.state_dir.rstrip('/')}/{name}"
        if (root / rel).is_file():
            out.append(rel)
    return [p for p in out if (root / p).is_file()]


def publish(repo: RepoConfig, lang: str, written: list[dict], git: Git | None = None,
            message: str | None = None) -> Published:
    """Commit this language's translations, and push when configured to."""
    result = Published(branch=repo.branch.format(lang=lang))
    if not repo.commit:
        result.skipped = "commit is disabled for this repo"
        return result
    if not written:
        result.skipped = "nothing was written"
        return result

    git = git or Git(repo.path)
    if not git.is_repo():
        raise GitError(f"{repo.path} is not a git repository; set commit = false to skip")

    result.paths = allowed_paths(repo, written)
    if not result.paths:
        result.skipped = "none of the written files exist on disk"
        return result

    subject = message or f"i18n({lang}): update {len(written)} translated file(s)"
    result.commit = git.commit(result.paths, result.branch, subject)
    if not result.commit:
        result.skipped = "the translations are already committed"
        return result
    if repo.push:
        git.push(repo.remote, result.branch)
        result.pushed = True
    return result
