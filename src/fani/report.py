"""Run reports.

Two artefacts per run: ``report.json`` for machines (the scheduler, a later
gate, a dashboard) and ``report.md`` for the human who has to decide what to do
about a ``needs_human`` result. Both are written even when the run fails, so a
failed run is never a silent one.
"""

import json
import platform
import time
from pathlib import Path

from .orchestrator import EXIT_CODES, LangOutcome, Status

_ORDER = [Status.ERROR, Status.NEEDS_HUMAN, Status.PARTIAL, Status.OK]

_LABEL = {
    Status.OK: "ok",
    Status.NEEDS_HUMAN: "needs human",
    Status.PARTIAL: "partial",
    Status.ERROR: "error",
}


def overall(outcomes: list[LangOutcome]) -> Status:
    """The worst status wins; an empty run is ok."""
    for status in _ORDER:
        if any(o.status is status for o in outcomes):
            return status
    return Status.OK


def exit_code(outcomes: list[LangOutcome]) -> int:
    return EXIT_CODES[overall(outcomes)]


def build(outcomes: list[LangOutcome], started_at: float, config_path: Path) -> dict:
    dispatch = [d for o in outcomes for d in o.dispatch]
    failed = [d for d in dispatch if not d.ok]
    return {
        "schema": 1,
        "config": str(config_path),
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(started_at)),
        "duration_s": round(time.time() - started_at, 3),
        "host": platform.node(),
        "status": overall(outcomes).value,
        "exit_code": exit_code(outcomes),
        "totals": {
            "languages": len(outcomes),
            "files_written": sum(len(o.written) for o in outcomes),
            "conflicts": sum(len(o.conflicts) for o in outcomes),
            "findings": sum(len(o.findings) for o in outcomes),
            "agent_calls": len(dispatch),
            "agent_failures": len(failed),
            "repair_rounds": sum(o.repair_rounds for o in outcomes),
            "remaining_tasks": sum(o.remaining_tasks for o in outcomes),
            "fuzzy_matched": sum(o.fuzzy_matched for o in outcomes),
            "commits": sum(1 for o in outcomes if o.published.commit),
        },
        "languages": [o.to_dict() for o in outcomes],
    }


def render(data: dict) -> str:
    t = data["totals"]
    lines = [
        f"# fani run — {data['status']}",
        "",
        f"- started: {data['started_at']} on {data['host']}",
        f"- duration: {data['duration_s']:.1f}s",
        f"- config: `{data['config']}`",
        f"- files written: {t['files_written']} · conflicts: {t['conflicts']} · "
        f"findings: {t['findings']}",
        f"- agent calls: {t['agent_calls']} ({t['agent_failures']} failed) · "
        f"repair rounds: {t['repair_rounds']} · reused from memory: {t['fuzzy_matched']}",
        "",
    ]

    if data["languages"]:
        lines += [
            "| repo | lang | status | written | findings | agent calls | commit | time |",
            "| --- | --- | --- | --- | --- | --- | --- | --- |",
        ]
        for lang in data["languages"]:
            label = _LABEL[Status(lang["status"])]
            pub = lang.get("published", {})
            commit = f"`{pub['commit'][:9]}` on `{pub['branch']}`" if pub.get("commit") else "—"
            lines.append(
                f"| {Path(lang['repo']).name} | {lang['lang']} | {label} | "
                f"{len(lang['written'])} | {len(lang['findings'])} | "
                f"{len(lang['dispatch'])} | {commit} | {lang['duration_s']:.1f}s |"
            )
        lines.append("")

    for lang in data["languages"]:
        if Status(lang["status"]) is Status.OK and not lang["findings"]:
            continue
        lines += [f"## {Path(lang['repo']).name} — {lang['lang']}", "", lang["message"] or "", ""]
        if lang["conflicts"]:
            lines.append("Hand-edited translations (not overwritten):")
            lines += [f"- `{c.get('path', c)}`" for c in lang["conflicts"]]
            lines.append("")
        if lang["findings"]:
            lines.append("Findings:")
            for f in lang["findings"][:40]:
                if isinstance(f, dict):
                    where = f.get("file") or f.get("path") or "?"
                    lines.append(
                        f"- `{where}` {f.get('severity', '')} {f.get('code', '')}: "
                        f"{f.get('message', '')}".rstrip()
                    )
                else:
                    lines.append(f"- {f}")
            if len(lang["findings"]) > 40:
                lines.append(f"- … {len(lang['findings']) - 40} more (see report.json)")
            lines.append("")
        failed = [d for d in lang["dispatch"] if not d["ok"]]
        if failed:
            lines.append("Failed agent calls:")
            lines += [
                f"- `{d['task_id']}` {d['code']}: {d['message']}" for d in failed[:20]
            ]
            lines.append("")

    return "\n".join(lines).rstrip() + "\n"


def write(outcomes: list[LangOutcome], out_dir: Path, started_at: float,
          config_path: Path) -> dict:
    data = build(outcomes, started_at, config_path)
    out_dir.mkdir(parents=True, exist_ok=True)
    (out_dir / "report.json").write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    (out_dir / "report.md").write_text(render(data), encoding="utf-8")
    return data
