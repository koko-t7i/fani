# fani

fani keeps translated documentation in sync with its source, unattended. It
watches a set of repositories, notices which documents changed, has a language
model translate the parts that need it, checks the result, and commits the
translations to a branch of their own.

It is the scheduler and the safety rails. The actual translation work is done by
the [i18n skill](https://github.com/your-org/i18n-skill), whose scripts decide
what to translate, split documents into chunks, protect code from the model, and
check the output. fani drives those scripts and calls an agent when a chunk of
text needs translating.

## Why it works this way

The model is used as a function, not as an operator. It receives one chunk of
source text and returns one chunk of translated text. It cannot read the
repository, write a file, run a command, or remember the previous call. Every
decision — what is out of date, whether an answer is acceptable, whether to try
again, what to commit — is made by code you can read in `src/fani/`.

That is what makes an unattended run safe to leave running on a timer.

## Install

fani needs Python 3.13, [uv](https://docs.astral.sh/uv/), git, a checkout of the
i18n skill, and at least one headless agent CLI (Claude Code, Codex, Grok, or
anything that reads a prompt on stdin and prints an answer).

```bash
uv tool install --from . fani
cp examples/fani.toml fani.toml   # then edit the paths
fani doctor                        # checks the config, the skill, uv and every agent
```

## Use

```bash
fani status    # what a run would do; calls no agent, costs nothing
fani sync      # translate everything that is out of date
fani doctor    # check that a run could succeed
```

`fani sync` writes `report.md` and `report.json` and exits with a code a
scheduler can act on:

| exit | meaning | what to do |
| --- | --- | --- |
| 0 | nothing to do, or translated and verified | nothing |
| 1 | a human has to look | read `report.md` |
| 2 | fani or the skill could not run | fix the config or the environment |
| 3 | this batch is done, more chunks remain | run again sooner |

Exit 1 happens when someone edited a translation by hand (fani never overwrites
that), when structural checks kept failing after the repair budget was spent, or
when the run looked like it was about to re-translate a whole repository.

## Run it on a schedule

`systemd/` holds a user service and timer; `systemd/fani.service` explains each
setting. The short version:

```bash
cp systemd/fani.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now fani.timer
```

Continuous integration is not where translation belongs — a run takes minutes to
hours and CI should not wait for it. Instead, CI checks the result after the
fact: `examples/github/i18n-gate.yml` verifies structure on every pull request,
including the one that carries fani's own translations.

## Configuration

`examples/fani.toml` is the annotated reference. The three things it decides:

- **repositories** — where they are, which languages, which files, and whether
  fani may commit;
- **agents** — the command line for each headless CLI, its concurrency, timeout
  and retries;
- **routing** — which agent handles translating, revising and proofreading.

Two settings exist to stop an unattended run from doing damage. `max_tasks`
caps how much one run translates, so a mistake is small and cheap. And
`full_retranslate_guard` stops a repository that already has translations from
suddenly needing dozens of fresh chunks, which almost always means the
translation memory was lost rather than that the documentation changed.

## How a run goes

1. **plan** — the skill compares every source against its translation and emits
   one task per chunk that needs work. A translation someone edited by hand is
   reported as a conflict and the run stops.
2. **dispatch** — each task becomes one agent call, run concurrently. Output is
   parsed, unwrapped and written to the file the skill expects. Failures are
   retried and recorded.
3. **apply** — the skill reassembles the chunks and writes the translated files.
   Anything that fails its structural checks is rejected, and the chunks of a
   rejected file get one more attempt.
4. **verify** — structure is checked again against the source. Failures go back
   to the agent with the specific finding attached, up to `repair_budget` times.
5. **revision** *(optional)* — a second agent reads source and translation
   together and judges meaning. Blocking findings stop the run.
6. **commit** — translations and the translation memory are committed together
   onto `i18n/<lang>`, using git plumbing so HEAD, the index and the working
   tree are left exactly as they were.

## Development

```bash
uv run --python 3.13 --no-project python -m unittest discover tests
uv run --with ruff ruff check .
```

The suite has no third-party dependencies and neither does fani at runtime. The
end-to-end tests drive the real skill with a scripted stand-in agent, so they
cover the skill's contracts without calling a model; they skip when the skill or
uv is not installed. Point `FANI_TEST_SKILL` at a skill directory to test a
different copy.
