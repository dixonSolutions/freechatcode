# Architecture v2 — provider-agnostic, harness-agnostic

Status: **proposal, grounded in verification**. This doc defines the target for
the multi-provider / multi-harness redesign. It builds on, and does not replace,
[docs/design.md](design.md) — the "who owns what" split there is still the law of
the project.

Why this exists: `freechatcode launch opencode` exited 1. The cause was not a
browser problem and not a relay problem. The launcher hardcodes the **Codewhale**
CLI contract (`--provider openai --model … --base-url … --api-key …`) and opencode
has none of those flags, so it died at argument parsing before ever calling the
relay. The same class of bug is waiting for every other target: the site is
hardcoded as "DeepSeek" and the agent is hardcoded as "Codewhale".

## The two axes of hardcoding

Everything wrong today falls into two axes, and the fix is two data-driven
abstractions — one per axis:

1. **The chat side is hardcoded to DeepSeek.** `config.rs` has a single `[chat]`
   URL + DeepSeek selectors (`ds-markdown`, `DeepThink`/`Search` chips); `lib.rs`
   serves exactly two models; `sessions.rs` has a column named
   `deepseek_chat_url`.
2. **The harness side is hardcoded to Codewhale.** `main.rs` spawns one binary
   with one flag shape; `setup.rs` only discovers "codewhale" binaries.

Dissolving axis 1 is a **Provider**; dissolving axis 2 is a **Harness adapter**.
Both are pure configuration plus a small amount of per-target glue, so a new
site or a new agent is a config entry or a small module, never a rewrite.

## Target module layout

The separation the user asked for, made explicit. Each box is one module with a
narrow interface; the arrows are the only paths they may call through.

```
main.rs        — CLI parsing, orchestration, wiring (thin)
  ├─ config.rs      — read/merge config layers (defaults → user → CLI/env)
  ├─ providers.rs   — Provider registry + per-site selectors/models (data + lookup)
  ├─ browser.rs     — Chromium lifecycle: launch/attach, keep-alive, liveness watch
  ├─ page.rs        — Playwright chat I/O: submit, read, settle, stream, recover
  ├─ relay.rs       — HTTP router: /v1/models, /v1/chat/completions, SSE, auth, routing
  ├─ harness.rs     — HarnessAdapter trait + per-harness spawn/config glue
  ├─ sessions.rs    — session→conversation link store + turn log (generalized)
  ├─ setup.rs       — per-provider/per-harness health + doctor/test commands
  └─ tui.rs         — pre-flight lobby
```

Most of `browser.rs` / `page.rs` / `relay.rs` / `sessions.rs` already exist inside
the current large `main.rs` and `lib.rs`. The refactor is extraction behind these
interfaces, not new behaviour. The reliability machinery that was hard-won —
reply detection against a virtualized transcript, `blame` classification, liveness
watch, crash recovery, the turn log — moves with its owner and stays per-tab.

## Provider abstraction

A `Provider` describes one free chat site. It is data, so it can be overridden in
the user config when a site changes its DOM (exactly as the DeepSeek selectors
are overridable today).

```toml
[[providers]]
id          = "deepseek"
name        = "DeepSeek Chat"
base_url    = "https://chat.deepseek.com"
allowed_hosts   = ["chat.deepseek.com"]
routed_url_pattern = "/a/chat/s/"   # a resumable conversation URL must contain this

[providers.selectors]
composer  = "textarea, [contenteditable='true'][role='textbox'], …"
assistant = ".ds-markdown, [data-message-role='assistant'], …"
send      = "button[type='submit'], button[aria-label*='send' i], …"
new_chat  = "button[aria-label*='new chat' i], …"
file_upload = "input[type='file'], …"

# A "model" is a set of page state. DeepSeek's model is selected by chips.
[[providers.models]]
id        = "deepseek-chat"
name      = "DeepSeek Chat"
toggles   = []

[[providers.models]]
id        = "deepseek-reasoner"
name      = "DeepSeek Reasoner"
toggles   = [{ selector = "div.ds-toggle-button:has-text('DeepThink')", on = true }]
```

- **`selectors`** generalizes the current `[selectors]` block verbatim; nothing
  here is new logic, only re-scoping to a provider.
- **`models`** generalizes the current `deepseek-chat` / `deepseek-pro` pair: a
  model is an id plus the page controls that must be engaged before a turn, and
  read back afterwards (the "read it back so a click that didn't land is a
  failure" rule carries over).
- A provider with one model and no toggles is the degenerate case, so the
  simplest site is still simple to describe.

### Gemini (target, not yet verified live)

`gemini.google.com` is reachable (HTTP 200). Gemini's model selection is a
dropdown, not a chip, so `models` needs a second toggle kind — `menu` with an
option locator — in addition to `chip`. That is the one genuinely new mechanism
the provider schema must express; everything else maps onto the existing
selectors. **Live verification is required before Gemini is claimed to work**;
Google surfaces are login-gated and bot-guarded more aggressively than DeepSeek.

### Google AI Mode (deferred)

`google.com/aimode` currently redirects to the generic search home
(`webhp?aep=11`) — it is not a stable, directly addressable chat surface today.
It is a *future* provider, not a v1 one. The architecture supports it the day it
becomes addressable; nothing in the core assumes a stable URL.

## Harness adapter abstraction

A `HarnessAdapter` knows how to turn a running relay into a spawned agent:

```rust
trait HarnessAdapter {
    /// Discover/resolve the harness binary (path, PATH lookup, remembered path).
    fn resolve(&self, spec: Option<&str>) -> Result<PathBuf>;
    /// Build the spawn contract: argv + env + any temp config file, so the
    /// harness talks to `base_url` with `token` and `model`.
    fn spawn_contract(&self, base_url: &str, token: &str, model: &str)
        -> Result<HarnessSpawn>;
    /// Interpret a non-zero exit (optional, for friendlier errors).
    fn describe_exit(&self, status: ExitStatus) -> String;
}
```

`HarnessSpawn` carries the argv, environment, and — for harnesses that read a
config file — a path to a generated config that is cleaned up after the run.

### Verified contracts

- **codewhale** — argv `--provider openai --model <id> --base-url <url>
  --api-key <token>`. This is what works today; the adapter is just the current
  `main.rs` spawn block moved here.
- **opencode 1.18.35** — no such flags. Config lives at
  `~/.config/opencode/opencode.jsonc`. The adapter generates a provider entry
  (schema verified against `https://opencode.ai/config.json`):

  ```json
  {
    "$schema": "https://opencode.ai/config.json",
    "provider": {
      "freechat": {
        "npm": "@ai-sdk/openai-compatible",
        "name": "FreeChat relay",
        "options": { "baseURL": "http://127.0.0.1:PORT/v1", "apiKey": "<token>" },
        "models": { "deepseek-chat": { "name": "DeepSeek Chat" } }
      }
    }
  }
  ```

  then spawn `opencode run --model freechat/deepseek-chat …`. The generated
  config is written to a temp location and passed via `OPENCODE_CONFIG`
  (or merged into the user's file with their existing keys preserved — the
  merge approach is friendlier and is what `doctor` will verify).
- **openclaw** (`openclaw/openclaw`) and **hermes** (`NousResearch/hermes-agent`)
  — same personal-assistant lineage, config-file driven, OpenAI-compatible
  providers. Both are *later* adapters; the trait is what makes them cheap. Their
  exact config keys will be verified against each project before an adapter
  ships.

The key discipline: an adapter only *points the harness at the relay*. It never
implements the agent loop — that is the harness's job, exactly as Codewhale's is
today. This preserves the split in `design.md` (harness owns the loop; wrapper
owns the move).

## Multi-model / multi-tab routing

The relay becomes a router instead of a single-model endpoint.

- One Chromium context owns a map of tab pages keyed by `(provider_id, model_id)`.
- `GET /v1/models` lists every model of every configured provider (each with
  `"pricing": "Unlimited Chat!"`, as today).
- Each `/v1/chat/completions` request carries a `model`; the router resolves
  `(provider, model)`, engages that model's page controls, and drives that tab.
- The session-link store generalizes: `chat_links` gains a `provider_id` column
  and the `deepseek_chat_url` column is renamed `chat_url`. One Codewhale/agent
  session can hold one link **per (provider, model)** it has used, so resuming
  returns to the right tab of the right site.
- Turn logging gains `provider_id` + `model_id` columns (the existing
  `model_label` stays as the page-attributed truth).

This is "many models at once on one interface": one browser, N tabs, each tab a
real conversation on a real free site, any of them selectable per request. It is
the existing single-tab machinery generalized, not a new concurrency model.

### What "at once" does *not* mean (pending confirmation)

Fanning a single prompt out to N models simultaneously (a best-of-N across free
models) is a different feature. It is possible on top of this router — N tabs, N
turns, one judge — but it is not v1. This doc assumes one turn → one model/tab,
selected per request.

## Config schema

The single `[chat]` / `[selectors]` block becomes a `[[providers]]` array plus a
`[harness]` block. Everything else (`[browser]`, `[timeouts]`, `[relay]`,
`[tools]`, `[transport]`) is unchanged. The DeepSeek values become the *committed
default provider* in `assets/config.default.toml`, so an existing install behaves
identically after migration.

```toml
[harness]
kind   = "codewhale"   # codewhale | opencode | openclaw | hermes
binary = "/path"       # optional override of discovery

[[providers]]
id = "deepseek"
# … as above …

# per-machine, additive — the user adds Gemini without touching the shipped file:
# [[providers]]
# id = "gemini"
# …
```

Precedence stays: CLI > env > user config > committed defaults.

## Setup, doctor, test

The "easy to set up, testable one at a time" requirement is a first-class command
surface, not documentation:

- `… doctor` — resolves the harness binary, checks the relay, and for each
  configured provider: launches the browser, waits for the composer, and reports
  reachable/authenticated/signed-out. Nothing is typed to a real conversation.
- `… test [provider]` — sends a trivial probe prompt to one provider and reports
  pass/fail with the raw evidence (first-chunk latency, model label, turn record).
- `… setup` — walks one provider at a time (resolve → composer → probe), so a
  user configures and verifies DeepSeek, then Gemini, without one disturbing the
  other.

These reuse the existing `health.rs` and the live-test harness rather than
duplicating browser logic.

## Migration & rename

The rename is the last, mechanical step, not the first:

1. Extract modules + introduce `Provider`/`HarnessAdapter` with DeepSeek +
   Codewhale as the only entries (behaviour identical; full test suite must stay
   green).
2. Add the opencode adapter (fixes the reported bug) and a `… doctor/test`
   surface.
3. Add the Gemini provider, verified live.
4. Generalize `sessions.rs` (rename columns, migrate the DB in place — the
   existing migrate-on-open pattern already does this safely).
5. Rename crate/CLI/config dir/docs in one commit once the name is locked.
6. openclaw/hermes adapters as verified.

Each step lands with its tests, so the thing never breaks between steps.

## Open risks

- **Gemini bot-guarding.** Login + consent walls may block headless driving. Must
  be verified live before claiming support; the provider schema already covers
  the selectors, but the *feasibility* is unproven.
- **Google AI Mode** is not currently a stable chat URL (redirects to search).
  Deferred.
- **opencode config merge.** Writing into `~/.config/opencode/opencode.jsonc`
  must preserve the user's existing keys; the merge-and-restore path needs a test.
- **Multi-tab memory.** N live chat tabs is heavier than one; `keep_alive`,
  liveness, and per-tab recovery must still behave when only one tab is used.

## Verification plan

- `cargo test --locked` (offline suite) stays green at every step.
- `cargo test --locked -- --ignored` live tests against DeepSeek stay green.
- New live tests: opencode spawns and answers through the relay; Gemini
  composer-detection + probe (when/if it can be driven); multi-tab (two tabs, two
  models, both answer).
- `doctor`/`test` exercised for real against at least one provider.
