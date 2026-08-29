"""One headless agent call = one subprocess: prompt on stdin, text back out.

The agent is a stateless work station. It never sees the repository, never
writes a file, and holds nothing between calls. Everything an invocation needs
arrives on stdin; everything it produces leaves on stdout (or, for CLIs like
codex that interleave event logs on stdout, through a temp file named by the
``{output_file}`` placeholder in the command template).

Timeouts kill the whole process group: agent CLIs fork helpers, and killing
only the direct child leaves those behind on a shared runner.
"""

import os
import signal
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

from .config import AgentConfig

OUTPUT_FILE_TOKEN = "{output_file}"


@dataclass(frozen=True)
class CallResult:
    ok: bool
    text: str  # stdout (or output-file contents) on success, diagnostic on failure
    code: str | None  # None on success, else DSP-TIMEOUT | DSP-EXIT | DSP-EMPTY
    duration_s: float
    exit_code: int | None = None


def _kill_group(proc: subprocess.Popen) -> None:
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(proc.pid, sig)
        except ProcessLookupError:
            return
        try:
            proc.wait(timeout=2)
            return
        except subprocess.TimeoutExpired:
            continue


def call_agent(agent: AgentConfig, prompt: str, cwd: Path | None = None) -> CallResult:
    """Run one agent invocation. Never raises for agent failure; returns a coded result."""
    out_path: Path | None = None
    cmd = list(agent.cmd)
    if any(OUTPUT_FILE_TOKEN in part for part in cmd):
        fd, name = tempfile.mkstemp(prefix="fani-out-", suffix=".txt")
        os.close(fd)
        out_path = Path(name)
        cmd = [part.replace(OUTPUT_FILE_TOKEN, str(out_path)) for part in cmd]

    start = time.monotonic()
    try:
        proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=str(cwd) if cwd else None,
            start_new_session=True,  # own process group, so timeouts kill forks too
            text=True,
        )
    except OSError as exc:
        if out_path:
            out_path.unlink(missing_ok=True)
        return CallResult(False, f"cannot start {cmd[0]!r}: {exc}", "DSP-EXIT", 0.0)

    try:
        stdout, stderr = proc.communicate(input=prompt, timeout=agent.timeout_s)
    except subprocess.TimeoutExpired:
        _kill_group(proc)
        proc.communicate()  # reap and drain
        if out_path:
            out_path.unlink(missing_ok=True)
        return CallResult(
            False, f"timed out after {agent.timeout_s:.0f}s", "DSP-TIMEOUT",
            time.monotonic() - start,
        )

    duration = time.monotonic() - start
    if proc.returncode != 0:
        tail = (stderr or stdout or "").strip()[-500:]
        if out_path:
            out_path.unlink(missing_ok=True)
        return CallResult(
            False, f"exit {proc.returncode}: {tail}", "DSP-EXIT", duration, proc.returncode
        )

    text = stdout
    if out_path is not None:
        text = out_path.read_text(encoding="utf-8") if out_path.exists() else ""
        out_path.unlink(missing_ok=True)

    text = normalise(text)
    if not text.strip():
        return CallResult(False, "agent produced empty output", "DSP-EMPTY", duration, 0)
    return CallResult(True, text, None, duration, 0)


def normalise(text: str) -> str:
    """Trim whitespace and defensively strip one wrapping ``` fence.

    Models occasionally wrap the whole answer in a code fence despite the
    prompt forbidding it. Only a single *outer* pair is removed; anything
    structural inside the translation is left for apply/verify to judge.
    """
    text = text.strip()
    if not text.startswith("```"):
        return text
    lines = text.splitlines()
    if len(lines) < 2 or lines[-1].strip() != "```":
        return text
    # Opening line must be a bare fence (```, ```markdown, ...), not content.
    if lines[0].strip("`").strip() not in ("", "markdown", "md", "json"):
        return text
    return "\n".join(lines[1:-1]).strip()
