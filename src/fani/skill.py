"""Thin subprocess wrapper around the i18n skill's ``run.sh``.

Every call goes through ``run.sh`` rather than the Python entry points directly.
That is not ceremony: ``run.sh`` supplies markdown-it-py through uv, and without
it the skill silently degrades to a regex fence scanner whose chunk boundaries
differ. Boundaries feed the chunk cache but ``CHUNKER_VERSION`` does not change,
so a degraded run poisons the cache invisibly.

``--state-dir`` is always passed explicitly. The skill otherwise infers it from
harness environment variables that do not exist under a scheduler.
"""

import json
import subprocess
from dataclasses import dataclass
from pathlib import Path


class SkillError(RuntimeError):
    """The skill could not run at all (exit 2, or output that is not JSON)."""


@dataclass(frozen=True)
class SkillResult:
    returncode: int
    data: dict
    stdout: str
    stderr: str


class Skill:
    def __init__(self, skill_dir: Path, root: Path, state_dir: str, timeout_s: float = 900.0):
        self.run_sh = Path(skill_dir) / "scripts" / "run.sh"
        self.root = Path(root)
        self.state_dir = state_dir
        self.timeout_s = timeout_s

    @property
    def state_path(self) -> Path:
        return self.root / self.state_dir / "state.json"

    def work_dir(self, run_id: str) -> Path:
        return self.root / self.state_dir / "work" / run_id

    def _run(self, args: list[str], want_json: bool) -> SkillResult:
        cmd = [str(self.run_sh), *args]
        try:
            proc = subprocess.run(
                cmd, capture_output=True, text=True, timeout=self.timeout_s,
                cwd=str(self.root),
            )
        except subprocess.TimeoutExpired as exc:
            raise SkillError(f"{' '.join(args[:2])} timed out after {self.timeout_s:.0f}s") from exc
        except OSError as exc:
            raise SkillError(f"cannot execute {self.run_sh}: {exc}") from exc

        data: dict = {}
        if want_json and proc.stdout.strip():
            try:
                data = json.loads(proc.stdout)
            except json.JSONDecodeError:
                if proc.returncode == 2:
                    raise SkillError(
                        f"{args[0]} failed (exit 2): {proc.stderr.strip() or proc.stdout.strip()}"
                    ) from None
                raise SkillError(
                    f"{args[0]} produced output that is not JSON: {proc.stdout[:400]}"
                ) from None
        if proc.returncode == 2:
            raise SkillError(f"{args[0]} failed (exit 2): {proc.stderr.strip()}")
        return SkillResult(proc.returncode, data, proc.stdout, proc.stderr)

    def _common(self) -> list[str]:
        return ["--root", str(self.root), "--state-dir", self.state_dir]

    def plan(
        self,
        lang: str,
        paths: tuple[str, ...] = (),
        exclude: tuple[str, ...] = (),
        max_tasks: int = 40,
        repair: Path | None = None,
    ) -> SkillResult:
        args = ["plan", *self._common(), "--lang", lang, "--max-tasks", str(max_tasks), "--json"]
        if paths:
            args += ["--paths", *paths]
        if exclude:
            args += ["--exclude", *exclude]
        if repair is not None:
            args += ["--repair", str(repair)]
        return self._run(args, want_json=True)

    def apply(self, run_id: str) -> SkillResult:
        return self._run(["apply", *self._common(), "--run", run_id, "--json"], want_json=True)

    def verify(self, lang: str) -> SkillResult:
        # Exit 3 means "nothing recorded for this language" -- not a failure.
        return self._run(["verify", *self._common(), "--lang", lang, "--json"], want_json=True)

    def review_plan(self, lang: str, mode: str, run_id: str | None = None) -> SkillResult:
        args = ["review", "plan", *self._common(), "--lang", lang, "--mode", mode, "--json"]
        if run_id:
            args += ["--run", run_id]
        return self._run(args, want_json=True)

    def review_collect(self, run_id: str) -> SkillResult:
        args = ["review", "collect", *self._common(), "--run", run_id, "--json"]
        return self._run(args, want_json=True)


def task_files(work: Path, kind: str = "tasks") -> list[Path]:
    d = work / ("review" if kind == "review" else "tasks")
    return sorted(d.glob("*.json")) if d.is_dir() else []


def findings_for_tasks(verify_data: dict, tasks: list[Path]) -> dict[str, str]:
    """Map task file stem -> the findings text for the file that task belongs to.

    Task ids are ``<slugified file+lang>.<chunk id>``; verify findings are keyed
    by source path. Matching on the slug prefix keeps repair prompts specific,
    which is the whole reason the repair template exists.
    """
    import re

    by_file: dict[str, list[str]] = {}
    for f in verify_data.get("findings", []):
        if f.get("severity") != "error":
            continue
        line = f"{f.get('code')}: {f.get('message')}"
        if "expected" in f:
            line += f"\n  expected={f.get('expected')!r}\n  actual  ={f.get('actual')!r}"
        by_file.setdefault(f["file"], []).append(line)

    out: dict[str, str] = {}
    for rel, lines in by_file.items():
        slug = re.sub(r"[^A-Za-z0-9]+", "-", rel).strip("-").lower()
        for t in tasks:
            if t.stem.startswith(slug):
                out[t.stem] = "\n".join(lines)
    return out
