"""Configuration: one human-written TOML file, validated loudly.

Humans write TOML (this file), machines write JSON (dispatch records, reports).
Every validation problem raises :class:`ConfigError` before a single model call
or repository mutation happens; the CLI turns it into exit 2.

Secrets never live here. Agent CLIs read their API keys from the environment.
"""

import tomllib
from dataclasses import dataclass, field
from pathlib import Path

STAGES = ("translate", "revision", "proofread")


class ConfigError(ValueError):
    """The config file is missing, malformed, or inconsistent."""


@dataclass(frozen=True)
class AgentConfig:
    """One headless agent CLI. ``cmd`` may contain ``{output_file}``, replaced
    per call with a temp file the CLI writes its final message to (codex-style
    retrieval); otherwise stdout is the output."""

    name: str
    cmd: tuple[str, ...]
    stages: tuple[str, ...] = ("translate",)
    concurrency: int = 6
    timeout_s: float = 300.0
    retries: int = 2
    enabled: bool = True


@dataclass(frozen=True)
class RepoConfig:
    path: Path
    languages: tuple[str, ...]
    paths: tuple[str, ...] = ()
    exclude: tuple[str, ...] = ()
    state_dir: str = ".claude/i18n"
    max_tasks: int = 40
    repair_budget: int = 2
    #: A repo that already has translations should never suddenly need this many
    #: fresh tasks. Tripping it usually means state.json was lost or corrupted,
    #: and proceeding would re-translate (and re-bill) the whole repository.
    full_retranslate_guard: int = 30
    branch: str = "i18n/{lang}"
    commit: bool = True
    push: bool = False
    remote: str = "origin"
    revision: bool = False
    proofread: bool = False


@dataclass(frozen=True)
class Config:
    skill: Path  # directory holding scripts/run.sh, assets/, references/
    repos: tuple[RepoConfig, ...]
    agents: dict[str, AgentConfig] = field(default_factory=dict)
    routing: dict[str, str] = field(default_factory=dict)

    def agent_for(self, stage: str) -> AgentConfig:
        """The enabled agent routed to ``stage``. Raises ConfigError when none is."""
        if stage not in STAGES:
            raise ConfigError(f"unknown stage {stage!r}")
        name = self.routing.get(stage)
        if name is None:
            for a in self.agents.values():
                if a.enabled and stage in a.stages:
                    return a
            raise ConfigError(f"no enabled agent handles stage {stage!r}")
        agent = self.agents.get(name)
        if agent is None:
            raise ConfigError(f"routing.{stage} points at unknown agent {name!r}")
        if not agent.enabled:
            raise ConfigError(f"routing.{stage} points at disabled agent {name!r}")
        if stage not in agent.stages:
            raise ConfigError(f"agent {name!r} does not list stage {stage!r} in its stages")
        return agent


def _require(table: dict, key: str, kind: type, where: str):
    if key not in table:
        raise ConfigError(f"{where}: missing required key {key!r}")
    value = table[key]
    if not isinstance(value, kind):
        raise ConfigError(f"{where}: {key!r} must be {kind.__name__}, got {type(value).__name__}")
    return value


def _str_tuple(value, where: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
        raise ConfigError(f"{where}: expected a list of strings")
    return tuple(value)


def _agent(name: str, table: dict) -> AgentConfig:
    where = f"[agents.{name}]"
    cmd = _str_tuple(_require(table, "cmd", list, where), f"{where}.cmd")
    if not cmd:
        raise ConfigError(f"{where}: cmd must not be empty")
    stages = _str_tuple(table.get("stages", ["translate"]), f"{where}.stages")
    for s in stages:
        if s not in STAGES:
            raise ConfigError(f"{where}: unknown stage {s!r} (known: {', '.join(STAGES)})")
    return AgentConfig(
        name=name,
        cmd=cmd,
        stages=stages,
        concurrency=int(table.get("concurrency", 6)),
        timeout_s=float(table.get("timeout_s", 300)),
        retries=int(table.get("retries", 2)),
        enabled=bool(table.get("enabled", True)),
    )


def _repo(index: int, table: dict) -> RepoConfig:
    where = f"[[repo]] #{index + 1}"
    path = Path(_require(table, "path", str, where)).expanduser()
    languages = _str_tuple(_require(table, "languages", list, where), f"{where}.languages")
    if not languages:
        raise ConfigError(f"{where}: languages must not be empty")
    stages = table.get("stages", {})
    if not isinstance(stages, dict):
        raise ConfigError(f"{where}: stages must be a table of booleans")
    return RepoConfig(
        path=path,
        languages=languages,
        paths=_str_tuple(table.get("paths", []), f"{where}.paths"),
        exclude=_str_tuple(table.get("exclude", []), f"{where}.exclude"),
        state_dir=str(table.get("state_dir", ".claude/i18n")),
        max_tasks=int(table.get("max_tasks", 40)),
        repair_budget=int(table.get("repair_budget", 2)),
        full_retranslate_guard=int(table.get("full_retranslate_guard", 30)),
        branch=str(table.get("branch", "i18n/{lang}")),
        commit=bool(table.get("commit", True)),
        push=bool(table.get("push", False)),
        remote=str(table.get("remote", "origin")),
        revision=bool(stages.get("revision", False)),
        proofread=bool(stages.get("proofread", False)),
    )


def load(path: Path) -> Config:
    """Parse and structurally validate ``fani.toml``. Filesystem checks that
    depend on the runtime environment live in :func:`check_environment`."""
    path = Path(path)
    if not path.is_file():
        raise ConfigError(f"config file not found: {path}")
    try:
        with path.open("rb") as fh:
            data = tomllib.load(fh)
    except tomllib.TOMLDecodeError as exc:
        raise ConfigError(f"{path}: {exc}") from None

    skill = Path(_require(data, "skill", str, str(path))).expanduser()

    repo_tables = data.get("repo", [])
    if not isinstance(repo_tables, list) or not repo_tables:
        raise ConfigError(f"{path}: at least one [[repo]] table is required")
    repos = tuple(_repo(i, t) for i, t in enumerate(repo_tables))

    agent_tables = data.get("agents", {})
    if not isinstance(agent_tables, dict) or not agent_tables:
        raise ConfigError(f"{path}: at least one [agents.<name>] table is required")
    agents = {name: _agent(name, t) for name, t in agent_tables.items()}

    routing = data.get("routing", {})
    if not isinstance(routing, dict):
        raise ConfigError(f"{path}: [routing] must be a table")
    cfg = Config(skill=skill, repos=repos, agents=agents, routing=dict(routing))

    # Routing consistency is a config error, not a runtime surprise.
    for stage in cfg.routing:
        if stage not in STAGES:
            raise ConfigError(f"[routing]: unknown stage {stage!r}")
        cfg.agent_for(stage)
    return cfg


def check_environment(cfg: Config) -> None:
    """Runtime checks that need the filesystem. Raises ConfigError on problems."""
    run_sh = cfg.skill / "scripts" / "run.sh"
    if not run_sh.is_file():
        raise ConfigError(
            f"skill not found: {run_sh} does not exist. `skill` must point at the "
            f"i18n skill directory that contains scripts/run.sh"
        )
    for repo in cfg.repos:
        if not repo.path.is_dir():
            raise ConfigError(f"repo path does not exist: {repo.path}")
