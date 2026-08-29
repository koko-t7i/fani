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
    slow        sleep past any sane timeout
    fail        exit non-zero
    review      print a findings JSON document
    flaky       fail on the first call, succeed afterwards (uses a counter file)
"""

import json
import os
import re
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
    prompt = sys.stdin.read()

    if mode == "slow":
        time.sleep(30)
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
