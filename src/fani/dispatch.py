"""Fan task files out to agent CLIs and write the results the skill expects.

The skill's ``plan`` writes one JSON task per chunk and expects one JSON result
per chunk at the ``result_path`` the task names. Nothing in between exists --
that gap is what this module fills, and it is the only place fani talks to a
model.

Two things are deliberately taken away from the agent:

*File access.* The task file names a ``result_path`` and the skill's own prompts
instruct a subagent to write it. fani's agents have no tools, so the dispatcher
overrides that instruction and writes the file itself from stdout.

*Prompt assembly.* ``plan`` already folded the glossary and style blocks into
each task's ``prompt`` field, so translation tasks forward it verbatim. Only
``revise`` and ``repair`` tasks need a template rendered around it.
"""

import json
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

from .adapters import call_agent
from .config import AgentConfig

#: Appended to every prompt. The skill's templates tell a subagent to write the
#: result file; our agents cannot, so the last instruction wins and says stdout.
_STDOUT_OVERRIDE = """

## Output channel (overrides any instruction above)

You have no file access. Do NOT attempt to write any file, and ignore any
instruction above telling you to write to a result path. Print your answer to
standard output and nothing else: no preamble, no commentary, no code-fence
wrapper around the whole answer.
"""

_REVISE_HEADER = """You are updating an existing translation, not writing a new one.

The previous source is in PREVIOUS SOURCE. Its approved translation is in
PREVIOUS TRANSLATION. The new source is in SOURCE. They differ by a match ratio
of {ratio}: close to 1.0 means very little changed.

1. Diff PREVIOUS SOURCE against SOURCE. Only those differences may change.
2. Start from PREVIOUS TRANSLATION and edit it. Every sentence whose source did
   not change must come through byte-identical. Do not re-word it, do not
   improve it, do not modernise it. It was approved.
3. Write new target-language prose only for what genuinely changed, matching the
   surrounding register, punctuation and terminology of the text you are editing.
4. If the source removed something, remove its translation. If the source added
   something, add a translation in the right position.

Everything in the original translation instructions below still applies:
placeholders byte-identical, Markdown structure unchanged, inline code verbatim,
no commentary.

## PREVIOUS SOURCE

{previous_source}

## PREVIOUS TRANSLATION

{previous_translation}

## ORIGINAL TRANSLATION INSTRUCTIONS

{prompt}

## SOURCE

{source}
"""

_TRANSLATE = """{prompt}

## SOURCE

{source}
"""

_REPAIR_HEADER = """You are repairing a translation that failed a structural check. Do not
re-translate from scratch; make the smallest change that fixes the listed problems.

## What was wrong with the previous attempt

{findings}

Rules, unchanged from the original task:

1. Keep every @@CODE_BLOCK_n@@, @@INLINE_CODE_n@@, @@LINE_nnnn@@ token byte-identical.
2. Copy every inline code span verbatim. Never translate anything between backticks.
3. Keep Markdown structure identical: same heading levels, same list nesting, same
   table shape, same number of links and images, same URLs.
4. Do not introduce HTML tags that are not in the source.

## ORIGINAL TRANSLATION INSTRUCTIONS

{prompt}

## SOURCE

{source}
"""


@dataclass(frozen=True)
class TaskOutcome:
    task_id: str
    ok: bool
    code: str | None
    attempts: int
    duration_s: float
    message: str = ""


def build_prompt(task: dict, findings: str | None = None) -> str:
    """Render the prompt for one task file. ``kind`` is inferred from the task."""
    if task.get("mode") == "revise" and task.get("previous_translation"):
        body = _REVISE_HEADER.format(
            ratio=task.get("match_ratio", "unknown"),
            previous_source=task.get("previous_source", ""),
            previous_translation=task["previous_translation"],
            prompt=task.get("prompt", ""),
            source=task.get("source", ""),
        )
    elif findings:
        body = _REPAIR_HEADER.format(
            findings=findings,
            prompt=task.get("prompt", ""),
            source=task.get("source", ""),
        )
    else:
        body = _TRANSLATE.format(prompt=task.get("prompt", ""), source=task.get("source", ""))
    return body + _STDOUT_OVERRIDE


def build_review_prompt(task: dict) -> str:
    """Review tasks arrive fully rendered by ``i18n_review.build_prompt``."""
    return task["prompt"] + _STDOUT_OVERRIDE


def _unwrap_translation(text: str, chunk_id: str) -> str:
    """Accept either a bare translation or the JSON envelope the skill's prompts ask for.

    The revise and repair templates request ``{"chunk_id", "translated_text"}``.
    A model that follows them literally must not be punished for it.
    """
    stripped = text.lstrip()
    if stripped.startswith("{"):
        try:
            data = json.loads(stripped)
        except json.JSONDecodeError:
            return text
        if isinstance(data, dict) and isinstance(data.get("translated_text"), str):
            if data.get("chunk_id") in (None, chunk_id):
                return data["translated_text"]
    return text


def _result_path(root: Path, task: dict) -> Path:
    """Where the skill will look for this task's result.

    ``result_path`` is recorded relative to the repository root by ``plan``.
    """
    return root / task["result_path"]


def _write_translation(root: Path, task: dict, text: str) -> None:
    path = _result_path(root, task)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"chunk_id": task["chunk_id"], "translated_text": text}
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def _write_review(root: Path, task: dict, text: str) -> bool:
    """Review results must be valid JSON; the skill's ``collect`` parses them."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return False
    if not isinstance(data, dict) or not isinstance(data.get("findings", []), list):
        return False
    path = _result_path(root, task)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return True


class Dispatcher:
    """Runs task files through an agent, bounded by that agent's concurrency."""

    def __init__(self, root: Path, agent: AgentConfig, log_path: Path | None = None):
        self.root = Path(root)
        self.agent = agent
        self.log_path = log_path
        self._log_lock = threading.Lock()

    def _record(self, entry: dict) -> None:
        if self.log_path is None:
            return
        with self._log_lock:
            self.log_path.parent.mkdir(parents=True, exist_ok=True)
            with self.log_path.open("a", encoding="utf-8") as fh:
                fh.write(json.dumps(entry, ensure_ascii=False) + "\n")

    def _run_one(self, task_file: Path, kind: str, findings: str | None) -> TaskOutcome:
        task = json.loads(task_file.read_text(encoding="utf-8"))
        task_id = task.get("task_id", task_file.stem)
        prompt = build_review_prompt(task) if kind == "review" else build_prompt(task, findings)

        started = time.monotonic()
        last = "unknown failure"
        code: str | None = "DSP-EMPTY"
        # attempt 0 plus `retries` more: transient failures (timeout, non-zero
        # exit) and contract violations both get another chance, because the
        # cost of one extra call is far below the cost of a half-translated file.
        for attempt in range(self.agent.retries + 1):
            result = call_agent(self.agent, prompt, cwd=self.root)
            if not result.ok:
                last, code = result.text, result.code
                continue
            if kind == "review":
                if _write_review(self.root, task, result.text):
                    return self._done(task_id, True, None, attempt + 1, started)
                last, code = "agent output was not valid review JSON", "DSP-EMPTY"
                continue
            text = _unwrap_translation(result.text, task.get("chunk_id", ""))
            if not text.strip():
                last, code = "agent output was empty after unwrapping", "DSP-EMPTY"
                continue
            _write_translation(self.root, task, text)
            return self._done(task_id, True, None, attempt + 1, started)
        return self._done(task_id, False, code, self.agent.retries + 1, started, last)

    def _done(
        self, task_id: str, ok: bool, code: str | None, attempts: int, started: float, msg: str = ""
    ) -> TaskOutcome:
        outcome = TaskOutcome(task_id, ok, code, attempts, time.monotonic() - started, msg)
        self._record({
            "task_id": task_id,
            "agent": self.agent.name,
            "ok": ok,
            "code": code,
            "attempts": attempts,
            "duration_s": round(outcome.duration_s, 3),
            "message": msg,
        })
        return outcome

    def run(
        self,
        task_files: list[Path],
        kind: str = "tasks",
        findings_by_task: dict[str, str] | None = None,
    ) -> list[TaskOutcome]:
        if not task_files:
            return []
        findings_by_task = findings_by_task or {}
        workers = max(1, min(self.agent.concurrency, len(task_files)))
        with ThreadPoolExecutor(max_workers=workers) as pool:
            futures = [
                pool.submit(self._run_one, f, kind, findings_by_task.get(f.stem))
                for f in task_files
            ]
            return [f.result() for f in futures]
