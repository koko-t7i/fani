"""Command line entry point.

``fani sync`` is what a scheduler runs. Its exit code is the whole interface to
the outside world:

===== ==========================================================
0     nothing to do, or everything translated and verified
1     a human has to look (conflicts, exhausted repairs, guard)
2     fani or the skill could not run (config, environment, bug)
3     partial: this batch is done, more chunks remain
===== ==========================================================

Exit 3 is deliberately not an error. A scheduler seeing 3 should simply run
again sooner; the run stopped at ``max_tasks``, not at a problem.
"""

import argparse
import shutil
import sys
import time
from pathlib import Path

from . import __version__
from .config import Config, ConfigError, RepoConfig, check_environment, load
from .lock import Lock, LockBusy
from .orchestrator import EXIT_CODES, LangOutcome, Orchestrator, Status
from .report import exit_code, write
from .skill import SkillError

DEFAULT_CONFIG = Path("fani.toml")


def _logger(quiet: bool):
    def log(msg: str) -> None:
        if not quiet:
            print(msg, file=sys.stderr, flush=True)

    return log


def _selected(cfg: Config, repo_filter: str | None) -> list[RepoConfig]:
    if not repo_filter:
        return list(cfg.repos)
    picked = [r for r in cfg.repos if repo_filter in (r.path.name, str(r.path))]
    if not picked:
        known = ", ".join(r.path.name for r in cfg.repos)
        raise ConfigError(f"no repo matches {repo_filter!r} (configured: {known})")
    return picked


def _languages(repo: RepoConfig, lang_filter: str | None) -> list[str]:
    if not lang_filter:
        return list(repo.languages)
    wanted = [lang for lang in repo.languages if lang == lang_filter]
    if not wanted:
        raise ConfigError(
            f"{repo.path.name} is not configured for {lang_filter!r} "
            f"(configured: {', '.join(repo.languages)})"
        )
    return wanted


def _inside(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent.resolve())
    except ValueError:
        return False
    return True


def cmd_sync(args: argparse.Namespace) -> int:
    started_at = time.time()
    log = _logger(args.quiet)
    cfg = load(args.config)
    check_environment(cfg)
    repos = _selected(cfg, args.repo)

    outcomes: list[LangOutcome] = []
    for repo in repos:
        languages = _languages(repo, args.lang)
        lock = Lock(repo.path / repo.state_dir / "fani.lock")
        try:
            lock.acquire()
        except LockBusy as exc:
            log(f"  {repo.path.name}: skipped, {exc}")
            outcomes.append(
                LangOutcome(
                    repo=str(repo.path), lang=",".join(languages),
                    status=Status.ERROR, message=str(exc),
                )
            )
            continue
        try:
            orch = Orchestrator(cfg, repo, log=log)
            for lang in languages:
                outcome = orch.run_language(lang)
                log(f"  {repo.path.name} [{lang}] {outcome.status.value}: {outcome.message}")
                outcomes.append(outcome)
        finally:
            lock.release()

    report_dir = Path(args.report_dir).resolve() if args.report_dir else Path.cwd() / ".fani"
    for repo in repos:
        if _inside(report_dir, repo.path):
            log(
                f"warning: reports are written inside {repo.path.name} ({report_dir}); "
                f"the next run will try to translate them. Move --report-dir outside the "
                f"repository, or add it to that repo's exclude list."
            )
    data = write(outcomes, report_dir, started_at, Path(args.config))
    log(f"report: {report_dir / 'report.md'}")
    if not args.quiet:
        print(data["status"])
    return exit_code(outcomes)


def cmd_status(args: argparse.Namespace) -> int:
    """Plan only. Reports what a sync would do without calling any agent."""
    cfg = load(args.config)
    check_environment(cfg)
    worst = Status.OK
    for repo in _selected(cfg, args.repo):
        orch = Orchestrator(cfg, repo)
        for lang in _languages(repo, args.lang):
            plan = orch.skill.plan(
                lang, repo.paths, repo.exclude, repo.max_tasks
            ).data
            conflicts = len(plan.get("conflicts", []))
            if conflicts:
                worst = Status.NEEDS_HUMAN
            print(
                f"{repo.path.name} [{lang}] tasks={plan.get('task_count', 0)} "
                f"files={len(plan.get('files', []))} conflicts={conflicts} "
                f"reused={plan.get('fuzzy_matched', 0)} "
                f"deferred={plan.get('truncated_tasks', 0)}"
            )
    return EXIT_CODES[worst]


def cmd_doctor(args: argparse.Namespace) -> int:
    """Check everything a run depends on, and report all problems at once."""
    problems: list[str] = []
    notes: list[str] = []

    try:
        cfg = load(args.config)
    except ConfigError as exc:
        print(f"FAIL config: {exc}")
        return 2
    print(f"ok   config: {args.config}")

    try:
        check_environment(cfg)
        print(f"ok   skill: {cfg.skill / 'scripts' / 'run.sh'}")
    except ConfigError as exc:
        problems.append(str(exc))
        print(f"FAIL environment: {exc}")

    if shutil.which("uv") is None:
        problems.append("uv is not on PATH; run.sh needs it to supply markdown-it-py")
        print("FAIL uv: not on PATH")
    else:
        print("ok   uv: on PATH")

    for name, agent in cfg.agents.items():
        state = "enabled" if agent.enabled else "disabled"
        binary = agent.cmd[0]
        found = shutil.which(binary)
        if found is None and agent.enabled:
            problems.append(f"agent {name}: {binary} is not on PATH")
            print(f"FAIL agent {name}: {binary} not on PATH")
        else:
            print(f"ok   agent {name} ({state}): {found or binary}")

    for stage in ("translate", "revision", "proofread"):
        try:
            print(f"ok   stage {stage} -> {cfg.agent_for(stage).name}")
        except ConfigError as exc:
            wanted = any(
                getattr(r, stage, stage == "translate") for r in cfg.repos
            )
            (problems if wanted else notes).append(f"stage {stage}: {exc}")
            print(f"{'FAIL' if wanted else 'note'} stage {stage}: {exc}")

    for repo in cfg.repos:
        lock = repo.path / repo.state_dir / "fani.lock"
        if lock.exists():
            notes.append(f"{repo.path.name}: a lock file exists at {lock}")
            print(f"note repo {repo.path.name}: lock present ({lock})")
        else:
            print(f"ok   repo {repo.path.name}: {', '.join(repo.languages)}")

    if problems:
        print(f"\n{len(problems)} problem(s) must be fixed before a run.")
        return 2
    print("\nready")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="fani", description="Scheduled documentation translation using the i18n skill."
    )
    parser.add_argument("--version", action="version", version=f"fani {__version__}")
    sub = parser.add_subparsers(dest="command", required=True)

    def common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--config", type=Path, default=DEFAULT_CONFIG, help="path to fani.toml")
        p.add_argument("--repo", help="only this repo (name or configured path)")
        p.add_argument("--lang", help="only this language")

    sync = sub.add_parser("sync", help="translate everything that is out of date")
    common(sync)
    sync.add_argument("--report-dir", help="where to write report.md/report.json (default .fani)")
    sync.add_argument("--quiet", action="store_true", help="suppress progress on stderr")
    sync.set_defaults(func=cmd_sync)

    status = sub.add_parser("status", help="show what a sync would do; calls no agent")
    common(status)
    status.set_defaults(func=cmd_status)

    doctor = sub.add_parser("doctor", help="check config, skill, uv and agent binaries")
    doctor.add_argument("--config", type=Path, default=DEFAULT_CONFIG, help="path to fani.toml")
    doctor.set_defaults(func=cmd_doctor)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except (ConfigError, SkillError) as exc:
        print(f"fani: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("fani: interrupted", file=sys.stderr)
        return 2


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
