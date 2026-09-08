# JSON browser consumer preparation

Test-only vanilla JS/Node HTTP fixture, not acceptance of fani JSON output. PR1/PR2 and the JSON backend remain prerequisites. There is no MDX fixture: its parser gate is blocked. Nothing here enables a format, changes a user repository, or adds Node/browser assets to native runtime or releases.

## Install and run

Use Node 24.18.0, npm 12.0.2, i18next 23.16.8 and Playwright 1.55.1. Chromium 140.0.7339.186 must already be provisioned in Playwright's browser cache (or provision it separately with explicit approval). No browser download runs in these scripts.

From the repository root:

```sh
cd tests/fixtures/frontend-consumer
npm ci --ignore-scripts
npm test
```

`npm test` creates and removes temporary **synthetic** candidates for harness negative tests. It does not invoke fani or demonstrate format acceptance.

For a candidate produced externally by the later Rust integration:

```sh
export FANI_CONSUMER_ROOT=/absolute/path/to/fixed-source-candidate
export FANI_CONSUMER_HASHES=/absolute/path/to/expected-sha256.json
npm run check
npm run test:ssr
npm run test:browser
# Optional interactive server, bound to loopback:
PORT=4173 npm start
```

No build step or bundler is needed. Browser tests own ephemeral server startup/teardown, launch Chromium, and fail if it is unavailable. `FANI_CONSUMER_ROOT` is required for check/start/SSR/browser, must be absolute, and must contain both `messages/en/common.json` and `messages/fr/common.json`. Source must match the seed's decoded structure. Missing targets fail before listening; no source-language or hand-authored target fallback exists. Resources are loaded once at server startup; restart after changing the candidate.

`FANI_CONSUMER_HASHES` is optional for harness exploration but **required for later real-fani acceptance**. It is a JSON object with exactly the two relative paths above as keys and lowercase hex SHA-256 digests of the exact candidate file bytes as values. The Rust caller must compute these from its actual fixed-source/generated-target artifacts, not from fixture reconstruction. Every runner logs consumed hashes; the server exposes the same hashes at `GET /resources.json` alongside the loaded resources. A byte-only change fails hash validation. Hash equality establishes identity, not fani provenance; the integration must record candidate revision, provider request/response and generation evidence separately.

## Recorded provider convention

Copy only `seed/` into an isolated temporary Git source repository. Later configure the JSON source mapping externally: source `messages/en/*.json`, strip prefix `messages/en/`, target `messages/{lang}/{relpath}`, locale `fr`, syntax `i18next-interpolation-v1`. This fixture deliberately supplies no config or Rust protocol adapter while PR2 is unfinished.

The deterministic test provider returns `FR: ` followed by each nonempty, non-whitespace decoded source text. When requests contain protected tokens, prefix the protected text and retain every token byte, order and occurrence; the assembler restores parameters. Empty/whitespace strings must be skipped and preserved, not prefixed. All keys, objects, array positions and number/boolean/null values remain unchanged. `expectedTarget` in `resources.mjs` is an assertion oracle only; delivery code never writes a target. Adapt the finalized request-v2/response-v1 protocol in Rust integration, not by inventing an interim API here. No live translation provider is needed for this convention.

The seed includes nested and array strings, repeated `{{name}}` / `{{ name }}`, dotted `{{user.name}}` / `{{ user.name }}`, quotes, newline, Unicode, empty/whitespace values and immutable scalars. Only the fixed default interpolation subset is used. No ICU, plurals, nesting, formatting, unescaped interpolation or rich-text resource syntax is claimed. Hostile markup appears only in dynamic test input. The prefix oracle intentionally does not test legal parameter reordering; backend tests own that broader contract.

## API and coverage

- `loadResources(root?)` requires external files, compares decoded content to the source/provider oracle, optionally verifies exact byte hashes, and returns `{resources, hashes}`.
- `translator(i18next, resources, locale)` creates an isolated instance with fallback disabled, empty strings retained, default interpolation escaping, and required-key diagnostics. Static resource text is escaped independently before interpolation.
- `render(instance, route, values?)` is shared by SSR and the browser. `startServer({root?, port?})` returns `{url, hashes, close}`.
- Four routes only: `/en/checkout`, `/fr/checkout`, `/en/account`, `/fr/account`. Both consumers share one browser instance and dynamic state. Input events update shared values; Apply renders them. Client navigation/language changes/back-forward retain values. Full reload resets to Alice/Élodie; no storage persistence is implied.
- SSR asserts all routes, both parameter sets, hostile dynamic escaping, empty/whitespace strings, repeated and dotted parameters, and missing-key failure.
- Chromium at desktop 1280×800 and mobile 390×844 performs direct loads, refreshes, real typing/submission/clicks, both language switches on both pages, route changes and back/forward. It checks all displayed fields, no stale/missing keys or raw braces, no injected elements/execution, and fails on page errors, console errors, failed requests or unexpected HTTP errors. Unknown routes return 404 and non-GET requests 405.
- Harness negatives cover absent env/target, malformed JSON, missing keys, lost parameters, scalar/array/empty mutation, and exact hash mismatch. This consumer uses `JSON.parse`, not a byte-preserving or duplicate-key validator. Unsupported syntax, duplicate keys, protected-token/schema verification, candidate persistence/publication blocking and format enablement remain Rust gates. This is not a substitute for any of them.

Dependency assessment commands (registry access required for current advisories):

```sh
npm audit --json
npm ls --all
node --input-type=module -e 'import fs from "node:fs"; const lock=JSON.parse(fs.readFileSync("package-lock.json")); for (const [path,p] of Object.entries(lock.packages)) console.log(path || ".", p.version, p.license);'
```

All npm dependencies are confined to this private test package. A clean audit is a registry snapshot, not a guarantee; report network failures rather than claiming a current clean result. No native Cargo or release checks are implied by fixture test success.
