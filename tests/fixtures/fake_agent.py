#!/usr/bin/env python3
"""A deterministic stand-in for a headless agent CLI.

Reads a prompt on stdin and prints a "translation" on stdout, preserving
everything the skill's verifier asserts: code placeholders, inline code, fence
count, heading structure. Behaviour is selected by the first argument so the
dispatcher's failure handling can be tested without a model.

Modes:
    ok          echo the SOURCE section back, marking prose lines
    empty       print nothing
    fenced      wrap the answer in a ```markdown fence (tests normalise())
    envelope    print the {"chunk_id", "translated_text"} JSON the prompts ask for
    mangle      corrupt the @@CODE_BLOCK_n@@ tokens (tests apply's rejection)
    verifyrepair fail verify once, then produce a structurally valid repair
    pause       hold a sync long enough to exercise the repository lock
    slow        sleep past any sane timeout
    forkhold    exit leader while a SIGTERM-ignoring child retains output pipes
    forkescape  keep a setsid descendant alive across process-group signals
    orphanok    succeed after detaching a background child
    noread      never read stdin, exercising prompt-write timeout
    flood       emit pipe-filling stdout and stderr beyond capture limits
    outputflood write an oversized answer to the configured output file
    fail        exit non-zero
    review      print a findings JSON document
    flaky       fail on the first call, succeed afterwards (uses a counter file)
"""

import json
import os
import re
import subprocess
import sys
import time

MARK = "[zh] "


def source_section(prompt: str) -> str:
    """Everything after the last '## SOURCE' heading, minus the output override."""
    idx = prompt.rfind("## SOURCE")
    body = prompt[idx + len("## SOURCE"):] if idx >= 0 else prompt
    cut = body.find("## Output channel")
    if cut >= 0:
        body = body[:cut]
    return body.strip("\n")


def translate(text: str) -> str:
    """Mark prose lines only. Structure, code and placeholders pass through."""
    out = []
    in_fence = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("```"):
            in_fence = not in_fence
            out.append(line)
            continue
        if in_fence or not stripped or re.fullmatch(r"@@[A-Z_]+_\d+@@", stripped):
            out.append(line)
            continue
        if stripped.startswith("#"):
            hashes, _, rest = line.partition(" ")
            out.append(f"{hashes} {MARK}{rest}" if rest else line)
            continue
        indent = line[: len(line) - len(line.lstrip())]
        out.append(f"{indent}{MARK}{stripped}")
    return "\n".join(out)


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "ok"
    if mode == "noread":
        time.sleep(30)
        return 0
    prompt = sys.stdin.read()

    if mode == "slow":
        time.sleep(30)
        return 0
    if mode == "orphanok":
        pid_file = os.environ["FAKE_AGENT_CHILD_PID"]
        child_code = (
            "import os, time; "
            "pid=os.fork(); "
            "os._exit(0) if pid else None; "
            "os.setsid(); "
            f"open({pid_file!r}, 'w', encoding='utf-8').write(str(os.getpid())); "
            "time.sleep(30)"
        )
        subprocess.Popen(
            [sys.executable, "-c", child_code],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 2
        while not os.path.exists(pid_file) and time.monotonic() < deadline:
            time.sleep(0.01)
        mode = "ok"
    if mode == "pause":
        time.sleep(1)
        mode = "ok"
    if mode == "forkslow":
        child = subprocess.Popen(["sleep", "30"])
        with open(os.environ["FAKE_AGENT_CHILD_PID"], "w", encoding="utf-8") as fh:
            fh.write(str(child.pid))
        time.sleep(30)
        return 0
    if mode in {"forkhold", "forkescape"}:
        pid_file = os.environ["FAKE_AGENT_CHILD_PID"]
        child_code = (
            "import os, signal, time; "
            + (
                "pid=os.fork(); os._exit(0) if pid else None; os.setsid(); "
                if mode == "forkescape"
                else ""
            )
            + "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            + f"open({pid_file!r}, 'w', encoding='utf-8').write(str(os.getpid())); "
            + "time.sleep(30)"
        )
        subprocess.Popen([sys.executable, "-c", child_code])
        deadline = time.monotonic() + 2
        while not os.path.exists(pid_file) and time.monotonic() < deadline:
            time.sleep(0.01)
        if mode == "forkescape":
            time.sleep(30)
        return 0
    if mode == "concurrency":
        import fcntl
        path = os.environ["FAKE_AGENT_CONCURRENCY_FILE"]
        with open(path, "a+", encoding="utf-8") as fh:
            fcntl.flock(fh, fcntl.LOCK_EX)
            fh.seek(0)
            parts = (fh.read().strip() or "0 0").split()
            active, maximum = int(parts[0]), int(parts[1])
            active += 1
            maximum = max(maximum, active)
            fh.seek(0); fh.truncate(); fh.write(f"{active} {maximum}"); fh.flush()
            fcntl.flock(fh, fcntl.LOCK_UN)
        time.sleep(0.2)
        with open(path, "r+", encoding="utf-8") as fh:
            fcntl.flock(fh, fcntl.LOCK_EX)
            active, maximum = map(int, fh.read().split())
            fh.seek(0); fh.truncate(); fh.write(f"{active - 1} {maximum}"); fh.flush()
            fcntl.flock(fh, fcntl.LOCK_UN)
        mode = "ok"
    if mode == "flood":
        sys.stderr.write("e" * (1024 * 1024))
        sys.stderr.flush()
        sys.stdout.write("x" * (5 * 1024 * 1024))
        return 0
    if mode == "outputflood":
        with open(sys.argv[2], "w", encoding="utf-8") as handle:
            handle.write("x" * (5 * 1024 * 1024))
        return 0
    if mode == "fail":
        sys.stderr.write("fake agent failing on purpose\n")
        return 3
    if mode == "empty":
        return 0
    if mode == "flaky":
        counter = os.environ.get("FAKE_AGENT_COUNTER", "")
        seen = 0
        if counter and os.path.exists(counter):
            seen = int(open(counter).read() or "0")
        if counter:
            with open(counter, "w") as fh:
                fh.write(str(seen + 1))
        if seen == 0:
            sys.stderr.write("transient failure\n")
            return 1
        mode = "ok"
    if mode == "review":
        print(json.dumps({"findings": []}, ensure_ascii=False))
        return 0

    text = translate(source_section(prompt))

    if mode == "mangle":
        text = re.sub(r"@@CODE_BLOCK_(\d+)@@", r"@@ CODE_BLOCK_\1 @@", text)
    if mode == "verifyrepair":
        counter = os.environ["FAKE_AGENT_COUNTER"]
        seen = int(open(counter).read() or "0") if os.path.exists(counter) else 0
        with open(counter, "w", encoding="utf-8") as handle:
            handle.write(str(seen + 1))
        if seen == 0:
            text = text.replace("# [zh] ", "## [zh] ", 1)
    if mode == "fenced":
        print("```markdown")
        print(text)
        print("```")
        return 0
    if mode == "envelope":
        chunk = re.search(r'"chunk_id":\s*"([^"]+)"', prompt)
        print(json.dumps(
            {"chunk_id": chunk.group(1) if chunk else "body:1", "translated_text": text},
            ensure_ascii=False,
        ))
        return 0

    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
