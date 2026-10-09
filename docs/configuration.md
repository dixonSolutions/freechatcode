# Configuration reference


Two layers, merged lowest-precedence first:

- **Compiled-in defaults** — `assets/config.default.toml`, embedded in the
  binary with `include_str!`. Ships with the package; not read at runtime.
- **User config** — `~/.codewhale/freechatcode/config.toml` (honors
  `$CODEWHALE_HOME`).

Resolution order for any value: **CLI flag → environment → user config →
compiled-in default**. So the codewhale path belongs in your user config
(`[codewhale] binary = …`), not in the repository.

### Bundle this in a mode

```toml
mode = "show"            # "show" = visible browser, prompts driven through the page
                        # "silent" = headless browser, prompts sent to the API directly
```

A mode is a **preset** over the two knobs below, applied under your own config:
`show` sets `[browser] headless = false` with `[transport] mode = "gui"`, and
`silent` sets `headless = true` with `transport.mode = "api"`. Setting either
knob yourself still wins — the one you leave alone follows the mode.

`silent` asks the site's own completion endpoint first, and that endpoint refuses
this project's requests (it wants a per-request proof-of-work header; see
[validation](../README.md#development) and [STATUS.md](../STATUS.md)). So when it refuses, the turn is retried
through the headless page and one line on stderr says so. The mode stays quiet
either way.

Sign-in is the one thing no mode suppresses: when the composer is not there, the
browser is reopened **visibly** in both modes so you can authenticate, and the run
continues afterwards. `--mode show|silent` overrides the config file.

### Browser

```toml
[browser]
mode = "managed"        # "managed" (own profile) or "attach" (your browser, over CDP)
headless = true         # false = always visible; true reopens a window if sign-in is needed
keep_alive = true       # false = close the browser after every turn
# profile_dir = "/home/you/.codewhale/providers/deepseek/browser"
# cdp_endpoint = "http://127.0.0.1:9222"
# record_video_dir = "/home/you/.codewhale/freechatcode/video"
# record_video_size = "1280x800"
```

`attach` reuses a browser you started yourself
(`chromium --remote-debugging-port=9222`) — the same signed-in session, no
separate profile. Passing `--cdp-endpoint` (or setting `cdp_endpoint`) implies
attach; you do not also need `mode = "attach"`. In `managed` mode the wrapper
refuses to fight another Chromium for the profile and says so in one line
instead of dumping a stack trace.

### Transport

```toml
[transport]
mode = "gui"            # "gui" drives the chat page; "api" POSTs from inside it

[transport.api]
# Private endpoint, driven from the page context so session cookies apply.
url = "/api/v0/chat/completion"
# Placeholders: {messages} {tools} {payload} {prompt} {thinking}.
# {thinking} is the reasoning flag a deepseek-pro turn needs — DeepSeek's own
# body calls it "thinking_enabled". A body without it cannot be told which model
# to use, so a pro turn over this transport is refused rather than answered by
# the plain model, and the turn falls back to the page instead.
body = "{\"messages\":{messages},\"stream\":true}"
framing = "sse"         # sse | json | text
text_path = "content"   # dot path into each response object
```

`api` mode is **not usable against DeepSeek**, and that is a measured finding
rather than an untested path. Reading the page's own traffic
(`cargo test -- --ignored live_discover_api_endpoint`) shows the endpoint is
`POST /api/v0/chat/completion` with the site's own body shape — and that every
request must carry, besides the bearer token, a per-request proof-of-work header
(`x-ds-pow-response`, from `/api/v0/chat/create_pow_challenge`) plus two
fingerprint headers. Without the token the server answers
`{"code":40003,"msg":"INVALID_TOKEN"}`; with it, `{"code":40300,
"msg":"MISSING_HEADER"}`. The page solves that challenge itself, which is
exactly what `gui` mode drives, so synthesizing it here is deliberately out of
scope. The mechanism is kept, complete and configurable, for endpoints that need
no such header.

### Providers

Each free chat site is a `[[providers]]` entry: its URL, its selectors, and the
models it serves. A model is page state (a reasoning chip, a dropdown), so two
models on one provider share one conversation. The shipped defaults define
DeepSeek; add Gemini (or any other chat UI) as a second entry:

```toml
[[providers]]
id = "deepseek"
name = "DeepSeek Chat"

[providers.chat]
url = "https://chat.deepseek.com"
allowed_hosts = ["chat.deepseek.com"]
routed_url_pattern = "/a/chat/s/"

[providers.selectors]
composer = "textarea, [contenteditable='true'][role='textbox'], [contenteditable='true']"
assistant = ".ds-markdown, [data-message-role='assistant'], [data-role='assistant']"
reasoning = ".ds-think-content"          # excluded from every read of the reply
send = "button[type='submit'], button[aria-label*='send' i], [data-testid*='send']"
new_chat = "button[aria-label*='new chat' i]"
thinking_toggle = "div.ds-toggle-button:has-text('DeepThink')"
model_label = "div.ds-toggle-button"   # what `freechatcode turns` records

[[providers.models]]
id = "deepseek-chat"
name = "DeepSeek Chat"
toggles = [{ selector = "div.ds-toggle-button:has-text('DeepThink')", on = false }]

[[providers.models]]
id = "deepseek-pro"
name = "DeepSeek Pro"
toggles = [{ selector = "div.ds-toggle-button:has-text('DeepThink')", on = true }]
```

`[[providers.models]]` declares what `/v1/models` advertises; a model's `toggles`
are the page controls engaged before a turn (and read back after). The selectors
were read off the live page, not guessed — `reasoning` in particular, which names
the page's own thinking block so it is never mistaken for the model's answer. Every provider runs as its own tab in
one relay. Use `--opt=all` to expose all configured providers to the harness;
the Gemini template uses the page-selected mode, advertised as `gemini`.

Timeouts are knobs too, and they matter: `poll_ms` (how often the page is
re-read) and `settle_polls` (how many identical reads mean "the reply stopped
growing") are what took the first turn from tens of seconds to about one. They
are not, however, how a long conversation is kept alive: a reply is spotted by
the newest message *changing*, so a conversation that has outgrown the page's
mounted window does not need a longer `response_secs`, only a correct read.

`settle_polls` is a **quiet window, not a completion check**, and it is the one
knob that trades latency against a truncated reply. The page renders in bursts
(measured: gaps of ~165 ms between deltas), so a window smaller than the longest
pause ends the turn mid-sentence — which is how a 992-character answer once came
back as 208 characters ending in the middle of a word. The shipped default is
~3 s, which costs a few seconds between the last word and the end of the turn;
lower it if you would rather have the speed and can afford the risk.

```toml
[timeouts]
login_wait_secs = 900
login_probe_secs = 20      # headless sign-in probe before reopening a visible window
response_secs = 300
poll_ms = 150
settle_polls = 20
action_secs = 20
navigation_secs = 20
link_probe_secs = 10
```

Tool forwarding is configurable by name — never hardcoded:

```toml
[tools]
forward_all = true                    # false: send only `essential` + `search`
essential = []                        # e.g. ["read", "edit", "write", "bash"]
search = ["tool_search"]
allow_extra = []                      # accept a tool call the request never declared
```

### Browser lifecycle

FreeChatCode watches the page and its browser context for closure and crashes, so
it can tell *"the browser is gone"* from *"the page is slow"* without waiting out
a timeout. Two things then recover it:

- **While idle**, a watcher (`[browser] liveness_check_secs`, default 15 s) checks
  the browser and reopens it if it has gone, so a browser that is closed or
  crashes comes back **by itself**, on the conversation in progress. Set it to `0`
  to turn the check off; it is skipped when `keep_alive = false`, where an absent
  browser is the point.
- **Mid-turn**, a turn that fails because the browser vanished is retried once
  against a freshly opened browser.

```
[idle] first reply="ONE"
[idle] conversation=https://chat.deepseek.com/a/chat/s/b60f366c-…
[idle] pkill status=exit status: 0
 freechatcode: the browser is gone; reopening it on the conversation and carrying on
 freechatcode: the browser is back
[idle] the browser came back by itself on …/a/chat/s/b60f366c-…
```

That is a real `SIGKILL` of the browser process, with **no turn sent in between**
— the browser returned on its own, to the same conversation. Both paths are
covered by live tests that run against a *copy* of the profile, so they never
compete with a session you have open.

Retries back off (up to 5 minutes), so a machine with no network does not spin,
and a non-browser failure — a refusal from the model, say — is reported rather
than retried.

If the whole wrapper is restarted, the conversation is still found: the
session→conversation link lives in `~/.codewhale/freechatcode/sessions.db`.

### The prompt

There is no wrapper-written prompt. The request body the harness sends — its
system message, its conversation, its declared tools — is handed to the chat page
as-is:

```json
{"messages":[…],"tools":[…],"tool_choice":…}
```

Nothing is prepended, appended or reworded. The harness already describes itself,
its workspace and its tools; a wrapper-authored preamble is the wrapper speaking
as the model, and a real session answered by reciting that preamble back. If a
model needs a different framing, that belongs in the harness's own prompt.

The page's own reasoning is not part of the reply. Chat UIs that render the
model's thinking above the answer (DeepSeek with DeepThink on) expose it in the
transcript, and it is excluded by selector:

```toml
[providers.selectors]
assistant = ".ds-markdown, [data-message-role='assistant'], [data-role='assistant']"
reasoning = ".ds-think-content"   # never read as the reply, never streamed
```

Set `reasoning = ""` for a page that renders none.


### Provider-specific authenticated browser

`chatmodels configure --Gemini --cdp-endpoint http://127.0.0.1:PORT` verifies
and saves that provider's native browser endpoint in `providers.chat.cdp_endpoint`.
A later launch or doctor check uses it without repeating the flag. Other
providers can keep managed profiles or attach to a different browser. An explicit
launch endpoint overrides the saved one. The attached browser must be running.

For DeepSeek native tool calls, `assistant_source_property = "markdown"` under
`providers.selectors` preserves exact code from the selected reply's original
React markdown property. Existing custom provider blocks need this setting
explicitly; an empty value reads rendered text. A configured source that cannot
be found produces an error rather than executing potentially altered code.
