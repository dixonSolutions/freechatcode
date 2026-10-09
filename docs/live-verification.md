# Live harness verification

Measured on 9 October 2026 using signed-in provider accounts, real installed
harnesses, isolated fixture workspaces, and independent Python test execution.
A zero harness exit code alone is insufficient: several failed coding runs
returned zero while leaving the source unchanged.

| Harness | Provider / model | Scenario | Harness exit / independent tests | Recording |
| --- | --- | --- | --- | --- |
| Codewhale 0.10.0 | DeepSeek Chat | Invoice | 0 / 2 pass | Rechecked after argument-preservation changes |
| OpenCode | DeepSeek Chat | Invoice | 0 / 2 pass | 25 seconds |
| OpenCode | DeepSeek Chat | Whitespace | 0 / 4 pass | [Main demo](../assets/demo.mp4), 45-second run |
| Official OpenClaw 2026.9.9 | DeepSeek Chat | Invoice | 0 / 2 pass | [Video](../assets/demo-openclaw.mp4), 33-second run |
| Official OpenClaw 2026.9.9 | DeepSeek Chat | Whitespace | 0 / 4 pass | [Video](../assets/demo-openclaw-whitespace.mp4), 38-second run |
| OpenCode compact agent | Gemini, page-selected model | Invoice | 0 / 2 pass | [Video](../assets/demo-gemini.mp4), 82-second run |
| OpenCode compact agent | Gemini, page-selected model | Whitespace | 0 / 4 fail | Model emitted malformed JSON; transport now reports a shape error |
| Codewhale, OpenCode, OpenClaw default agents | Gemini | Invoice | 0 / 2 fail each | Model returned prose without tool execution |
| Codewhale | Google AI Mode | Invoice | 0 / 2 fail | Before the guard: composer silently truncated context |
| Official OpenClaw | Google AI Mode | Invoice | 1 / 2 fail | Guard rejects 38,616-character request truncated to 8,192 |
| OpenCode response agent, tools disabled | Google AI Mode | Exact token response | 0 / exact response passes | [Video](../assets/demo-google-ai-mode-response.mp4), 11-second run |
| OpenCode compact agent with minimal project tool | Google AI Mode | Invoice | 0 / 2 fail | [Diagnostic video](../assets/demo-google-ai-mode-diagnostic.mp4), 14-second run |

Google AI Mode also answered a small plain-text token probe correctly, then
passed the exact-response check through the real OpenCode harness with a
project-owned agent whose tools were disabled. Its
current coding requests can return web-search results or “no response available.”
The visible refusal is recognized promptly. These results do not establish
working coding-tool support for AI Mode.

## Authentication and isolation

Google rejected sign-in in automated Chromium. Authentication succeeded in
normal Chromium using a dedicated profile, then the same browser/profile was
restarted with a loopback CDP port. Both Google products had signed-in composers.
Copying that profile to another Chromium application did not retain sign-in.

`chatmodels configure --Gemini --cdp-endpoint …` and its GoogleAIMode counterpart
both verified the native authenticated composer in an isolated configuration
home. They leave the attached browser open and save the endpoint per provider;
subsequent doctor checks succeeded without a CDP flag. An actual OpenCode
`--opt=all` launch warmed DeepSeek, Gemini, and Google AI Mode (1.3, 0.7,
and 0.7 seconds respectively) and returned its requested exact token.

The Cargo `openclaw-cli` 0.1.0 package was installed and inspected separately;
it exposes gateway/configuration commands and has no `agent` command. The
bridge rejects it before browser startup. Official OpenClaw was installed
without onboarding or a daemon. The adapter uses a temporary configuration and
isolated temporary runtime state; no user OpenClaw configuration is overwritten.
The invoice scenario passed again after runtime-state isolation was added.

## Reproduce

Build the bridge and install the desired harness. Start from a signed-in
provider profile; use CDP for an already authenticated native browser.

```bash
cargo build --locked
python3 tools/record_demo.py --harness opencode --provider deepseek --scenario slug
python3 tools/record_demo.py --harness opencode --provider gemini --compact-agent \
  --cdp-endpoint http://127.0.0.1:PORT
python3 tools/record_demo.py --harness openclaw --provider deepseek --scenario slug
python3 tools/record_demo.py --harness opencode --provider google-ai-mode \
  --compact-agent --minimal-tool --cdp-endpoint http://127.0.0.1:PORT
python3 tools/record_demo.py --harness opencode --provider google-ai-mode \
  --scenario response --compact-agent --cdp-endpoint http://127.0.0.1:PORT
```

The compact agent is project-owned OpenCode configuration, with a concise tool
reply contract and bash-only permission. `--minimal-tool` additionally installs
the example project tool in [tools/fixtures](../tools/fixtures/opencode-bash.ts),
which executes real shell commands through OpenCode. It did not make AI Mode
perform the coding task. These are explicit harness settings, not instructions
injected by the bridge.

Captures retain timestamped process output, command, baseline failures, exit
code, and independent verification. Default rendering refuses failed captures.
`--diagnostic` permits a clearly titled failed-run recording. The response scenario checks the actual parsed harness output against an
expected token and does not claim file editing or fabricate a failing baseline.
New attached-browser
recordings wait for the unique fixture workspace path, avoiding existing tabs
and unrelated newly opened pages.

The main clip combines real browser footage and timestamped harness output at
normal speed. Wrapper diagnostics are omitted from its terminal pane. MP4s are
1920×864; GIFs are reduced previews. Supplementary recordings preserve their
own provider/harness labels.

## Issues and poll

The thinking-output discussion poll had two votes, both to ignore page reasoning.
Reasoning exclusion remains enabled. The issue screenshots and the 50-second
video attached to issue #5 were inspected alongside current rendered demo frames.

The active suite has 99 passing tests and 23 ignored live tests.
Current transport regressions cover exact native code arguments, supported
complete envelope variants, context truncation before send, temporary OpenClaw
state cleanup, and malformed JSON tool-call rejection. Provider/model output
limitations and automatic Codewhale subagent identity remain tracked separately.

A real OpenCode task invocation created an explore child session. Audit input
and SQLite links confirmed the main and child share a parent session ID while
using distinct agent IDs, chat URLs, and fresh child history. The default child
then returned a promise instead of reading files, and the parent timed out;
this proves identity isolation, not successful delegated work. A compact-agent
attempt returned malformed JSON and was explicitly rejected. Automatic
Codewhale identity remains open because its OpenAI client does not send a
per-agent identity.
