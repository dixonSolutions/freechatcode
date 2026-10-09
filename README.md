# FreeChatCode 🌉

Run a coding harness through your signed-in chat browser. FreeChatCode connects
Codewhale, OpenCode, or OpenClaw to a local OpenAI-compatible relay, then drives
the provider's web UI with Playwright. The harness runs tools in your workspace;
the browser supplies model replies.

```text
Codewhale / OpenCode / OpenClaw
              ↓
   authenticated loopback relay
              ↓
     signed-in Chromium chat
```

You use your own chat account. Its subscription, usage limits, and availability
still apply. No provider API key is required for the browser transport.

## Get started

```bash
cargo install --git https://github.com/dixonSolutions/freechatcode --locked
freechatcode install                  # install Codewhale if needed
freechatcode                         # visible browser and Codewhale
```

Sign in directly in the browser when prompted. Playwright's matching Chromium
is downloaded automatically if missing. To install the browser beforehand:

```bash
cargo run --bin freechatcode-install-browser
```

Run from the directory where the harness should work. Pass harness arguments
after `--`:

```bash
freechatcode launch codewhale -- --help
freechatcode launch opencode -- run "Inspect this project and run its tests"
freechatcode launch openclaw -- --json "Inspect this project and run its tests"
```

OpenCode and official OpenClaw must already be installed. The Cargo package
`openclaw-cli` 0.1.0 is a community gateway CLI without an `agent` command; it
cannot run this coding adapter. Use [official OpenClaw](https://docs.openclaw.ai/install).

## Demos

![A real coding run through the browser relay](assets/demo.gif)

[Watch the full OpenCode + DeepSeek demo](assets/demo.mp4): normalize whitespace,
edit Python, and pass four tests. The terminal pane contains timestamped
output from the real harness; the browser pane records the actual chat page.
The fixture starts with failing tests and is checked independently after the
harness exits. The publishing script refuses failed runs by default.

[Record another example](tools/record_demo.py) and
[render the capture](tools/render_demo.py):

```bash
cargo build --locked
python3 tools/record_demo.py --harness opencode --provider deepseek --scenario slug
python3 tools/render_demo.py /path/printed/by/recorder/capture.json
```

| More examples | Scenario | Independent result |
| --- | --- | --- |
| [OpenCode + Gemini](assets/demo-gemini.mp4) | Invoice fix with the explicit compact agent | 2 tests pass |
| [Official OpenClaw + DeepSeek](assets/demo-openclaw.mp4) | Read, edit, execute | 2 tests pass |
| [OpenClaw whitespace scenario](assets/demo-openclaw-whitespace.mp4) | Trim, repeated spaces, tabs, empty input | 4 tests pass |
| [OpenCode + Google AI Mode response](assets/demo-google-ai-mode-response.mp4) | Small request with tools disabled | Exact response check passes |
| [Google AI Mode diagnostic](assets/demo-google-ai-mode-diagnostic.mp4) | Explicitly failed coding run | No tool call; 2 tests still fail |

Gemini's compact agent is an OpenCode project configuration, selected with
`--compact-agent` in the recorder. It permits bash only and explains the tool
reply shape in the harness's own prompt. The bridge adds no instructions.
See [OpenCode's agent configuration](https://opencode.ai/docs/agents/).

## Providers and harnesses

| Provider | Browser responses | Coding tools |
| --- | --- | --- |
| DeepSeek | Verified with a signed-in account | Real file edits and passing tests with all three harnesses |
| Gemini | Verified with a signed-in native browser | OpenCode compact-agent invoice scenario passed; default agents in all three harnesses returned prose |
| Google AI Mode | Verified through OpenCode with a small agent | Coding remains experimental; search fallbacks and context limits affect requests |

Provider model IDs describe page state. Gemini uses the model selected in its
web UI. DeepSeek exposes `deepseek-chat` and `deepseek-pro`; the latter engages
DeepThink. Provider web pages can change independently of this project.

```bash
freechatcode chatmodels list
freechatcode chatmodels configure --Gemini
freechatcode chatmodels configure --GoogleAIMode
freechatcode chatmodels set-default --Deepseek
freechatcode launch opencode --chatmodel=Gemini
freechatcode launch opencode --opt=all
```

Configuration checks the composer before saving the provider. That confirms
browser readiness; successful tool execution needs a live harness test.
Defaults persist. `--opt=all` exposes all configured models and warms their
browsers. See [configuration](docs/configuration.md) for selectors and model
settings.

## Browser and authentication

The default `show` mode opens a visible browser. `silent` runs headless; its
API attempt falls back to the page when the provider refuses it. For headless
browser transport without that attempt, use:

```toml
mode = "silent"
[transport]
mode = "gui"
```

If Google rejects sign-in from automated Chromium, authenticate in a normal
supported browser first. Then restart that same profile with a loopback CDP
port and attach using `--cdp-endpoint http://127.0.0.1:PORT`. This flag also
works with `chatmodels configure` and `doctor`. Configure saves the endpoint
for that provider, so later launches can reuse its sign-in while that browser
is running. Other providers retain their own browser settings. Keep the same
browser application: copying cookies between Chromium applications can lose
authentication because their encryption differs. Credentials are entered by
you; the bridge checks the resulting composer.

Useful flags include `--mode show|silent`, `--profile-dir`, `--cdp-endpoint`,
`--chatmodel`, `--model`, `--harness`, and `--harness-bin`. For a managed browser,
`--record-video DIR --record-video-size 960x800` records the driven page.
Attached browsers can be recorded with [the capture helper](tools/capture_browser.cjs).

## Conversations, tools, and files

The bridge forwards the harness's messages and tool catalog without adding
instructions. It translates JSON tool calls and DeepSeek's native DSML calls
to OpenAI tool calls. For native calls, the original selected reply's markdown
preserves code whitespace that the rendered DOM would otherwise change.
Page reasoning is excluded from replies, following the
[discussion poll](https://github.com/dixonSolutions/freechatcode/discussions/11).

A prose response ends the harness turn, including a promise to act or a claim
that tools are unavailable. The bridge carries that response through; it does
not invent a tool call. Live failures are tracked in
[issue #16](https://github.com/dixonSolutions/freechatcode/issues/16).

Requests can identify independent conversations with
`x-freechatcode-session-id` and `x-freechatcode-agent-id`, or a `conversation`
object with `session_id` and `agent_id`. OpenCode's native session and parent
headers are recognized. Each identity gets its own tab, history, and replay
cache. Tabs expire after five idle minutes, with a limit of 64 per provider.
Harnesses without agent identity share the launch conversation; automatic
Codewhale subagent identification remains [issue #1](https://github.com/dixonSolutions/freechatcode/issues/1).

SQLite stores conversation URLs and message metadata for resume. Explicit
image data URLs and local file parts upload through the provider's file input,
up to 16 files and 8 MiB combined. Remote URLs and file IDs are forwarded as
references. The harness retains ownership of workspace files and permissions.

## Configuration and troubleshooting

Defaults are embedded from [assets/config.default.toml](assets/config.default.toml).
User overrides live at `~/.codewhale/freechatcode/config.toml`, or under
`$CODEWHALE_HOME/freechatcode`. CLI options override configuration.

```bash
freechatcode health
freechatcode turns --limit 20
freechatcode feedback --title "Reproducible failure" --body-file report.md
```

The feedback command creates a GitHub issue. Include the harness/provider,
steps, expected result, and relevant diagnostic output. Audit logs contain
request and reply content; review them before sharing. The relay binds to
loopback and uses a random bearer token; local audit and database files use
owner-only permissions.

For detailed settings, recovery behavior, and timeouts, read
[the configuration reference](docs/configuration.md). Historical measurements
and test evidence live in [STATUS.md](STATUS.md). Ownership rules are in
[the design document](docs/design.md).

## Development

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Live tests are ignored by default and require signed-in provider profiles.
Run a selected live test with `cargo test NAME -- --ignored --nocapture`.
The demo recorder uses isolated workspaces, requires a failing baseline, and
checks the resulting files with an independent test process. The renderer
refuses failed captures by default; `--diagnostic` labels a failure explicitly.
Current validation: **99 active tests pass**, with 23 live tests ignored by
default. See [the live harness matrix](docs/live-verification.md) for measured
results and remaining limitations.

## License

[MIT](LICENSE)
