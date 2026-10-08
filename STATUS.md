# STATUS

What DeepChatCode can do, what was verified, and what was not. Last updated
2026-10-08 on the machine described under [Environment](#environment).

The short version: the bridge works end to end. A real `codewhale` 0.10.0
process was driven through the local relay, the browser, and the live DeepSeek
Chat page, and printed its answer. Every core criterion below was exercised
against `chat.deepseek.com`, not a mock, and the raw output is quoted.

The one failure a user kept hitting — "mysterious silence around the 3rd–4th
prompt" — turned out to be the wrapper's own reply detection, not DeepSeek:
the chat page virtualizes its transcript, so the element count it was keyed on
stops growing exactly when a conversation gets long enough. See criterion 18.

One designed-in limitation was found and is documented with evidence: the
`api` transport cannot work against DeepSeek because the site requires a
per-request proof-of-work header. See [Gaps](#gaps--what-is-not-verified).

## Reproduce the checks

```bash
cargo test --locked                            # 72 tests, offline, no provider call
cargo test --locked -- --ignored --nocapture   # live tests: real browser, real chat page
cargo clippy --locked --all-targets            # 0 warnings
cargo fmt --check
```

The live tests need a signed-in Chromium profile at
`~/.codewhale/deepseek-chat/browser`. They open a browser and talk to DeepSeek
for real. `live_attach_mode_round_trip` additionally needs a Chromium listening
on `127.0.0.1:9222`:

```bash
chromium --headless=new --remote-debugging-port=9222 \
  --user-data-dir="$HOME/.codewhale/deepseek-chat/browser" about:blank &
```

## Criteria

| # | Requirement | Status | Evidence |
| --- | --- | --- | --- |
| 1 | Resume continues the already-linked chat, no new chat, no re-fed transcript | **Verified live** | After a real turn the wrapper captured `…/a/chat/s/93e26d4c-…`; a second browser opened on that URL reported `link recognised on resume=true`, answered a follow-up, and the URL never changed. Unit tests cover the tail-only prompt. |
| 2 | Session management is fast | **Verified** | Resolution reads one JSON per session file and picks the newest match; no message bodies are scanned. |
| 3 | Chat can call tools and get results back through the relay | **Verified live** | Live HTTP through the real router returned `finish_reason: "tool_calls"`, `read_file`, `{"path":"src/main.rs"}` in **1.94 s**. The full agent run below shows the same path with a real listing. |
| 4 | From a chat, identify which model was used for what | **Verified live** | The page's own chips are read per turn (`DeepThink=off, Search=on`) and every turn is written to `chat_turns`, printed by `deepchatcode turns`. |
| 5 | Output is chunked as it is written, not only at the end | **Verified** | A test asserts ordered `delta` chunks plus one `finish_reason` chunk; live turns delivered growing snapshots, and a partial JSON envelope is decoded as it arrives. |
| 6 | Initial prompt recognition + submit are fast | **Verified live** | **923 ms** to the first streamed chunk; **1.44 s** for the whole turn. Zero stalls in the full agent run. Earlier in this work the same turn took 6.3 s, and the previous session reported 30–45 s. |
| 7a | Config: kill Playwright after a turn or keep it | **Verified live** | `live_turn_closes_browser_when_keep_alive_is_off` asserts the session is dropped after a reply, that the next turn relaunches it, and that it is dropped again. Two turns answered (`PONG`, `OK`). |
| 7b | Config: headless vs headful | **Done, and it was broken** | `[browser] headless` was previously **ignored** — the code hardcoded a visible window. Now honoured (all full-agent runs below were headless), with an automatic visible reopen when the sign-in probe fails. |
| 7c | Config: native/existing Chromium session or not | **Verified live** | `live_attach_mode_round_trip` attached to a Chromium the wrapper did not launch (`attached to https://chat.deepseek.com/`) and answered `PONG` in it. **Found and fixed a bug here**: `--cdp-endpoint` did not imply attach — you also had to set `mode = "attach"`, contradicting the flag's own help text. |
| 7d | Config: web-app GUI vs the domain's API directly | **GUI verified live; API blocked by the site — diagnosed with evidence** | See [Gaps](#gaps--what-is-not-verified). The endpoint, request shape, and error codes were captured from live traffic. |
| 8 | Test everything | **Done** | 72 tests pass; 19 live tests are `#[ignore]`d by default, including three that run a real agent end to end, and all were run explicitly. `clippy` clean, `fmt --check` clean, release build OK. |
| 9 | Local image-analysis model + local OCR | **Verified** | `tools/vision.py` (local `gemma4:26b` via Ollama) and `tools/ocr.py` (tesseract 5.5.3, model fallback). OCR read a 30 px known-text render byte-exact on all three backends. |
| 10 | Desktop remote control | **Enabled; authentication verified end to end** | GNOME Remote Desktop RDP on `*:3389`, TLS, NLA, view-only off. FreeRDP 3.32.1 authenticated successfully with the stored password (exit 0, no `ERRCONNECT_LOGON_FAILURE`) while a wrong password was rejected (`0x00020014`, server logged `client authentication failure`). |
| 11 | Demo video, frames checked, README | **Done** | `assets/demo.gif` + `assets/demo.mp4`, 4.96 s, from a real recorded turn; frames read back with the local vision model and they show the prompt, the search phase, and the finished answer. |
| 12 | README titled "poor man's deepseek api" with a wallet emoji, plus status and capabilities | **Done** | See `README.md`. |
| 13 | STATUS.md covering capabilities and criteria | **Done** | This file. |
| 14 | Commit nothing useless or sensitive | **Nothing committed — not a git repo** | The workspace has no `.git`. No secrets are in the tree; `.codewhale/` runtime state (pastes, transcripts) is gitignored, as are the session DB and user config. |
| 15 | Persistent development and testing | **Done** | `cargo test --locked` is the offline gate; the `#[ignore]`d live tests are the end-to-end gate. Both are checked into `src/main.rs`. |
| 16 | Track browser events; survive the browser being closed or crashing | **Verified live** | The page and its context are watched for `close` and `crash`, so liveness needs no round trip. Two live tests, both on a *copy* of the profile: `live_browser_reopens_on_the_same_conversation_after_a_kill` `SIGKILL`s the browser mid-session and the next turn reopens it on the same conversation and answers (`ONE` → kill → `TWO`); `live_browser_comes_back_while_idle_without_a_turn` kills it and then sends **no turn at all**, and the idle watcher still brings it back on the same conversation. The decision is unit-tested too: a dead browser is recoverable, a model refusal is not. |
| 17 | The turn record is maintained; failures are recorded and diagnosed | **Verified live** | `chat_turns` now records failed turns as well as answered ones, with `outcome`, `failure_kind`, `blame`, `http_status` and `detail`, plus a migration so an older database gains the columns instead of losing the fields silently. Live proof: pointing the bridge at `deepchatcode-test.invalid` produced `net::ERR_NAME_NOT_RESOLVED` → `FAILED (dns, blame=network)` in the log, with no session at all. The one rule that matters is tested both ways: an unresolvable name is never `blame=service`, and an HTTP status always is. |
| 18 | A conversation keeps answering however long it gets | **Verified live; found and fixed** | The page virtualizes its transcript. Past a few exchanges the mounted window fills and it unmounts an old message in the same render it mounts the new one, so the assistant element count **stops growing** — and the old "one more element than before" test then called a page that had answered *silent*, so every turn died at the 300 s budget. Measured on the live page: a four-prompt conversation left only 2 assistant elements mounted (`count .ds-markdown = 2`) with 4 answers on screen. Detection now keys on the newest reply's **text**, not the count. Live proof — six padded turns in one conversation: `W1`…`W6`, each in ~1.9 s, with the mounted count standing still (`3 -> 3`) on turns 4, 5 and 6 and every reply still read. The test refuses to pass unless at least one turn really did answer without the count growing. |
| 19 | Two run modes: `show` and `silent` | **Verified** | `mode = "show"` (default) means a visible browser with prompts driven through the page; `mode = "silent"` means headless with prompts sent to the site's API. A mode is a *preset applied under the user config*, so a knob set explicitly still wins — unit-tested in both directions (an explicit `headless = false` survives `silent`; the knob the user left alone follows the mode). `--mode` overrides the file. Sign-in reopens a visible window in both modes. Live: `live_silent_mode_answers_headless_through_the_fallback` answers `QUIET` with no window. |
| 20 | A pro model, not just a pro name | **Verified live, through the relay** | `/v1/models` advertises `deepseek-chat` and `deepseek-pro`, both served; any other id is still refused (`the_pro_model_is_served_and_a_stranger_is_still_refused`, which also asserts the reply names the model that was asked for). `deepseek-pro` is not a second endpoint — it is the page with its DeepThink control engaged, and the toggle is *driven*, read back, and put back. Live, directly: `live_the_pro_model_engages_the_pages_reasoning_mode` read the chips `DeepThink=off → on → on through a real turn (reply "PRO") → off`. Live, through the real router: `live_relay_engages_pro_for_a_pro_request` POSTs a `deepseek-pro` request and reads the chips back `off → on`, then POSTs a `deepseek-chat` request and reads them `on → off`, with the replies `ELEVEN`/`TWELVE` and `model` reported as `deepseek-pro`/`deepseek-chat`. And live on the path that was almost a hole: `live_silent_mode_still_honours_a_pro_request` runs `silent` + pro and reads back `chips="DeepThink=on, Search=on"` with the reply `THIRTEEN`, after the api template refused because it cannot express reasoning. A pro turn that cannot engage the control fails rather than answering as the plain model. |
| 21 | Pricing is answered, and never "unknown" | **Wrapper: verified. Codewhale's footer: not reachable — measured, see gap 7** | Every model the wrapper advertises carries `"pricing": "Unlimited Chat!"` plus numeric zeros, asserted by `both_models_are_advertised_with_a_price_that_is_never_unknown`. That is the whole of what this wrapper is asked about pricing, and it never answers null, empty or absent. What *Codewhale* prints in its own footer is Codewhale's computation from its own catalog, and the five shapes tried against real Codewhale did not move it. |
## Live evidence (raw)

### The whole pipeline: real codewhale → relay → browser → DeepSeek

```
$ deepchatcode -- exec "Reply with exactly the word PONG and nothing else."
Using codewhale binary: ~/.cargo/bin/codewhale
Browser relay audit: …/audit/session-5c5ff5b361ed4e80b02216054dc6c93e.jsonl
Sign in directly in the browser window if needed; the wrapper never reads credentials.
Starting Codewhale with the DeepSeek Chat browser route.
The local relay listens only on 127.0.0.1:33665; no browser CORS access is enabled.
Platform credentials and cookies remain in the browser profile.
PONG
EXIT=0
```

`codewhale 0.10.0` was launched by the wrapper, pointed at the loopback relay,
and answered from the chat page. This is the objective, end to end.

### Tool forwarding, A/B against the live model

Same request (`exec --auto "List the files in the current directory using a tool."`),
only `[tools]` changed, read back out of the relay's own audit records:

| | `forward_all = false` | `forward_all = true` |
| --- | --- | --- |
| Codewhale declared | 12 tools | 12 tools |
| Forwarded in the payload | **0** | **12** |
| Browser prompt | 1267 bytes | 34709 bytes |
| Tool-call turns | 0 | 1 |
| Outcome | the model said the tools array was empty and answered in prose | it listed the real directory (`alpha.txt — 6 bytes`, `beta.txt — 5 bytes`) |

### Tool call through the relay

```
[live relay] tool-call turn (1.937840954s):
  {"choices":[{"finish_reason":"tool_calls","index":0,
   "message":{"content":null,"role":"assistant","tool_calls":[
     {"function":{"arguments":"{\"path\":\"src/main.rs\"}","name":"read_file"},
      "id":"call_1","type":"function"}]}}]}
```

### Streaming, speed, resume, model label

```
[live] first snapshot at Some(923.506265ms)
[live] total wall time: 1.443047624s
[live] model label=Some("DeepThink=off, Search=on")
[live] conversation url=https://chat.deepseek.com/a/chat/s/93e26d4c-591f-436e-acde-c1f1d1814bf0
[live] link recognised on resume=true
[live] resumed reply="OK"
```

### Why long conversations went silent:
`cargo test -- --ignored --nocapture live_replies_are_read_after_the_mounted_window_fills`

```
[window] turn 1: 1.747718286s mounted assistant elements 0 -> 1, reply="W1"
[window] turn 2: 1.935772509s mounted assistant elements 1 -> 2, reply="W2"
[window] turn 3: 1.836844954s mounted assistant elements 2 -> 3, reply="W3"
[window] turn 4: 1.829740906s mounted assistant elements 3 -> 3, reply="W4"   <- count froze
[window] turn 5: 1.935286867s mounted assistant elements 3 -> 3, reply="W5"   <- count froze
[window] turn 6: 1.914267844s mounted assistant elements 3 -> 3, reply="W6"   <- count froze
[window] mounted assistant elements per turn: [(0,1), (1,2), (2,3), (3,3), (3,3), (3,3)]
```

Six turns in one conversation, each answered in under two seconds, while what
the page keeps mounted slides and then stops growing:

```
[window] count configured assistant selector = 3      outline:
[window] count .ds-markdown = 3                         [0] ds-assistant-message-main-content :: W4
[window] count [class*=ds-markdown] = 6                 [2] ds-assistant-message-main-content :: W5
[window] count [class*=message] = 9                     [4] ds-assistant-message-main-content :: W6
```

The turn before the fix would have stopped noticing at turn 4 and then waited
out all 300 s on turns 4, 5 and 6 — the reported "silence around the 3rd–4th
prompt". The same saturation is visible in the session that reported it: its
4th prompt is recorded `FAILED (page_silent, blame=unknown)` with
`assistant_elements=3, before=3`, and the page's answer to that turn was still
sitting on the conversation when it was opened and read (641 chars, beginning
"If you mean the regular DeepSeek chat app or web interface …").

### The two modes and the pro model

```
$ cargo test -- --ignored --nocapture live_the_pro_model_engages_the_pages_reasoning_mode
[pro] page chips at rest: "DeepThink=off, Search=on"
[pro] after chat: "DeepThink=off, Search=on"
[pro] after pro: "DeepThink=on, Search=on"
[pro] pro turn reply="PRO"
[pro] page chips after the pro turn: "DeepThink=on, Search=on"
[pro] after switching back: "DeepThink=off, Search=on"
test result: ok. 1 passed; 0 failed; finished in 7.40s
```

```
$ cargo test -- --ignored --nocapture live_silent_mode_answers_headless_through_the_fallback
deepchatcode: the direct API path refused (the api transport returned no text
  (HTTP 200, 48 bytes): {"code":40003,"msg":"INVALID_TOKEN","data":null});
  using the headless page for this turn instead
[silent] reply="QUIET"
test result: ok. 1 passed; 0 failed; finished in 5.85s
```

Note the shape of that refusal: **HTTP 200** carrying an error envelope. A status
is proof the service answered; it is not proof it answered *successfully*.

One flake worth naming rather than smoothing over: a cold headless open has twice
failed on `[timeouts] navigation_secs` (20 s) under load — once on the silent test
and once on `live_the_pro_model_engages_the_pages_reasoning_mode`, each passing
unchanged on the next run. It is the page taking longer than 20 s to load, not the
wrapper misbehaving, and the wrapper's own navigation uses the same 20 s budget:
a genuinely slow load would fail a real turn the same way. The lever is
`[timeouts] navigation_secs`; it is left at its default here because raising it
would hide slow loads rather than report them, and twenty seconds is already
generous for a page that then answers a turn in two.

### `silent` plus a pro request, which was almost a silent lie

```
$ cargo test -- --ignored --nocapture live_silent_mode_still_honours_a_pro_request
deepchatcode: the direct API path refused (the page is in pro mode but
  [transport.api] body has no {thinking} placeholder, so the endpoint cannot be
  told which model to use; add {thinking} to the body template, or run with
  [transport] mode = "gui"); using the headless page for this turn instead
[silent+pro] reply="THIRTEEN" chips="DeepThink=on, Search=on"
test result: ok. 1 passed; 0 failed; finished in 5.23s
```

The api path cannot be told which model to use unless its body template names
`{thinking}`, and DeepSeek's own body carries `"thinking_enabled"`. Rather than
answer as the plain model while the caller believes it asked for pro, that
combination now refuses and falls back to the page — where DeepThink is engaged
for real, which the chips above are read back to prove.

### A real agent, doing real work, through the bridge

The split this project rests on — wrapper as a model endpoint, the page holding the
thread, Codewhale owning the loop — is verified with nothing mocked. The shipped
wrapper binary starts, launches real Codewhale, drives a real browser on a copy of
the profile, and each test pins one property:

```
$ cargo test --locked --bin deepchatcode -- --ignored --nocapture

live_a_real_agent_reads_a_file_through_the_bridge
[agent] exit=Some(0) | tool read completed: The token is MAGIC-TOKEN-7F3A.
finished in 13.40s / 13.64s          (run twice)

live_a_two_step_tool_loop_survives_the_round_trip
[loop] exit=Some(0) tools_reported=2 stdout="…LOOP-TOKEN-3B58"
finished in 19.89s

live_a_one_shot_run_leaves_no_session_to_inherit
[one-shot] file on disk: "THREAD-TOKEN-9C41"
[one-shot] codewhale sessions before=47 after=47
[one-shot] second run: "I do not know. … no record of prior actions in this session."
finished in 21.52s
```

The tokens exist only inside the files the agents read, so nothing passes unless a
tool ran, its result went back into the page's thread, and the answer came out of
that same thread. A two-step task shows the thread carrying a tool result into the
next decision. The third records a boundary rather than a wish: a one-shot
`codewhale exec` saves no session, `--session-id` resumes rather than creates one,
so two independent one-shot runs cannot recall each other — and the wrapper starts
clean instead of pretending otherwise. Cross-run memory is a session feature, which
an interactive `deepchatcode launch codewhale` has and a one-shot does not. The
design is written out in [docs/design.md](docs/design.md).

That suite is also what forced the last change to the contract: the first version
asked for a tool in a request that declared none, the model called one anyway, and
the wrapper's name check answered **502** — killing the whole agent run. The
wrapper now enforces the wire *shape* (a name, arguments that are a JSON object)
and carries the call, because Codewhale is the authority on its own tools and can
answer the model itself. A model's guess must not be able to kill a run.

### Cold start, and where it went

The wrapper used to open the browser **before** starting Codewhale, and said why in
a comment — "a stale link then surfaces immediately instead of on the first turn".
That put the entire cold start in front of the TUI the user is watching.

- Codewhale is now spawned first and the browser is warmed **in parallel**. Nothing
  needs the page until the first turn. The diagnosis is not traded away: a warm-up
  that cannot open the browser still stops the run, with the same classified
  message and the same turn record, as soon as the failure is known.
- Navigation now waits for `DOMContentLoaded` instead of Playwright's default
  `load`. The page is a heavy single-page app, so `load` waits for every font,
  image and analytics beacon; the wrapper only needs the DOM, because it polls for
  the composer itself a moment later.

Measured on this machine, cold profile copy, same page:

```
                              before      after
browser ready, headless       2.7-6.6 s   1.8-2.2 s
browser ready, visible        10.1 s      5.5 s
Codewhale in the terminal     after it    0.00 s
```

Reproduce with a profile copy, so a live session keeps its own browser:

```bash
target/release/deepchatcode --mode silent --profile-dir <copy> -- exec "Reply with exactly the word PING and nothing else."
```

The wrapper prints both numbers on every start — `Codewhale started at Ns` and
`browser ready in Ns` — so this ordering is visible in the logs, and a regression
in it is not silent.

One self-inflicted bug is worth recording, because measurement is what caught it:
the warm-up reported failure over a `oneshot`, and a sender dropped when the task
finishes reads as an error on the receiving end — so a *successful* warm-up looked
like a failure and stopped the run after a good turn. Only an actual message counts
as a failure now.

Worth knowing when choosing a mode: `show` is the default and it is also the
slowest start, because a visible Chromium is a visible Chromium (5.5 s against
1.8 s). `mode = "silent"` with `[transport] mode = "gui"` is the fast quiet
combination — headless, but the prompts still go through the page rather than
starting with an api call that this site refuses.

### The faster start handed the terminal back in the wrong mode

The parallel start above traded something away that only a report found: moving the
browser's launch *behind* Codewhale's startup also moved its damage behind the TUI.
The report was "strange symbols when I moved my cursor in the terminal".

An `LD_PRELOAD` interposer that logs every `tcsetattr` in the whole process tree
named the writer in one run — and it was not the browser, and not Codewhale:

```
31006.9659 pid=771513 comm=deepchatcode  tcsetattr fd=0(/dev/pts/1) ECHO=1 ICANON=1 ISIG=1
t=0.16s  echo=0 icanon=0 isig=0   <- Codewhale's TUI sets raw mode, correctly
t=0.33s  echo=1 icanon=1 isig=1   <- our own process writes the cooked state back
```

`playwright-rs` ships a defensive termios guard (its fix for issue #59): at the
first `Playwright::launch()` it snapshots **stdin's** line discipline, and
`Drop for Playwright` writes that snapshot back. Before the parallel start the
browser came up first and the write-back landed harmlessly; after it, the snapshot
is taken while the terminal is still cooked and restored *after* the TUI has set raw
mode. `ECHO` back on means the line discipline echoes the TUI's input — which is why
cursor movement painted `^[[<35;10;5M` over the UI.

The guard can only read and write fd 0, so the fix is to stop giving it a terminal
there: `take_terminal_off_stdin()` points the wrapper's fd 0 at `/dev/null` before
the browser exists and hands Codewhale the terminal explicitly (`Stdio::from` of a
dup of the old fd 0). The wrapper never reads stdin — its own TUI talks to
`/dev/tty` through crossterm — so nothing is lost.

Measured, same interactive session, a virtual mouse moving over the terminal:

```
                         ECHO at 5s   screen lines carrying report text
fix disabled (before)    True         1
fix present              False        0
bare Codewhale (control) False        0
```

And the same A/B through the entry point the report named (`deepchatcode tui`,
lobby → `q` → bridge → Codewhale's TUI), where the damage lands immediately after
the handover:

```
                                              lobby up   after q   after 8 reports
deepchatcode.prefix (fix disabled)  raw        cooked     cooked  -> 1 screen line:
    ???^[[<0;10;5M^[[<3;12;7M^[[<35;10;5M^[[<0;10;5M^[[<3;12;7M^[[<35;10;5M^[[<0;10;5Mntials.
deepchatcode.fixed                  raw        raw        raw     -> 0 screen lines
```

The reports are painted *inside* the app's own text — that `ntials.` is the tail
of "the wrapper never reads credentials" — which is exactly the signature in the
original report.

`LD_PRELOAD` trace after the fix: zero `tcsetattr` calls from `comm=deepchatcode`,
against two before it. Codewhale's own TUI renders identically to the bare control,
so the explicit terminal costs the child nothing.

Honest limit: the fix depends on the crate touching fd 0 and nothing else. The
crate's own comments say it has never pinned down which subprocess damages the
tty, so if a future version opened `/dev/tty` directly the handover would stop
covering it. The pty regression test is the guard on that; the handover is not.

### The tool call was there — and the parser threw it away

The first version of the test above was bad evidence: its prompt was one I wrote
to elicit a tool call ("Check the repo and tell me"), which proves the prompt *can*
elicit one and nothing about whether the relay *keeps* one. Replaced with the
reported message verbatim, and run three times. That changed the diagnosis:

```
run 1: finish_reason="stop" content="Here is the answer.\n\nHappy to dig into all of that..."
run 2: finish_reason="stop" content="... Let me read a couple of files.
       {"type":"tool_calls","tool_calls":[{"id":"call_1",...}]}"
run 3: finish_reason="stop" content="Here is the answer.\n\nHappy to dig into all of that..."
```

Run 2 is the real bug. The model **did decide to act** — it wrote its reasoning
and then appended the envelope — and the relay required the *whole reply* to be
JSON, so the call was discarded and the JSON was printed at the user as prose.
The tool never ran. Nothing was logged as a failure, because nothing failed: the
relay delivered exactly what it had been handed.

Fixed by finding the envelope wherever it is: a brace-balanced scan for the last
`tool_calls` object (fenced, trailing, or amid prose), with the prose kept as the
message's content — content *and* tool calls together is what a native tool-call
turn looks like. Three deterministic tests, one using the run-2 shape verbatim,
fail against the old parser and pass against this one.

Two more things worth writing down honestly:

- Runs 1 and 3 **answered** rather than acting, and that is not a defect: the
  message asked to *discuss* a name and some features. A discussion and a stall
  look identical in one reply, so the relay only re-asks when a turn that was
  offered tools comes back with prose that is not marked as a final answer. If a
  model writes "Here is the answer. Let me look.", that is accepted — the relay
  cannot decide intent it was not given.
- The live test now asserts the *guarantee* (the turn ends on an action or on a
  marked answer, never on an unmarked promise) and prints which happened, because
  a live model is stochastic. The deterministic guarantee lives in the offline
  tests, where a model cannot influence the outcome.

### And then the bridge stalled the agent, by my own change

Bridging Codewhale's briefing (§ above) gave the model a prompt that reads like a
*native* tool-calling setup, and the transport contract became a footnote. A real
session then did this:

```
Here is the answer.

I want to check two things before I give you a straight opinion: the repo's own
naming footprint ... and whether Gemini's app / "AI mode" has any API-level
support ... Let me look.
```

...and the turn ended there. Prose is a **final answer** on this transport, so
Codewhale's loop stopped with nothing done and no error raised — from the user's
seat, "there is no loop feeding it". The model announced an action instead of
emitting the tool-call object, and the relay faithfully reported what it was sent.

The contract is now explicit, is named as not-style, and is repeated **after** the
payload where recency matters most:

- never announce an action — "let me look" is not an action, and a reply that is
  not the JSON object is a final answer, so the turn ends and nothing runs;
- never claim a tool ran.

Pinned live, which is the only way a claim about model behaviour can be pinned:

```
$ cargo test -- --ignored --nocapture live_a_tool_invitation_becomes_a_call_not_a_promise
[loop] finish_reason="tool_calls" message={"content":null,"role":"assistant",
       "tool_calls":[{"function":{"arguments":"{\"path\":\"README.md\"}",
       "name":"read"},"id":"call_1","type":"function"}]}
test result: ok. 1 passed; 0 failed; finished in 12.62s
```

Harder instructions are not a guarantee — a model can still drift — so the honest
reading is: the contract is stronger and the failure has a test that catches it,
not that narration is impossible. A mechanical net (one nudge-retry when a prose
reply arrives while tools were supplied and it reads as a promise to act) is
possible and bounded, but it is a heuristic that can misfire on a short legitimate
answer, so it is not in this tree until someone asks for it.

### The model stopped describing the wrapper

Reported from a real session: asked something ordinary, the model answered
"I'm reached through the DeepSeek Chat web page, driven in a browser you're signed
in to..." and recited the session handshake instead of doing anything. That text
was the wrapper's own instruction file talking about itself. Same kind of bare
message now, through the relay, with the briefing bridged:

```
[coder] reply="I don't have a tool catalog or any tool definitions in this
 request — tools is null, and the system message only contains a marker where
 those would go. So there's nothing I can actually call. If you want me to use
 tools, send the catalog (names, descriptions, and argument schemas) along with
 the request."
test result: ok. 1 passed; 0 failed; finished in 9.70s
```

The model read its input and said what was missing from it. The test also reads
the audit back to assert Codewhale's briefing really was bridged, and refuses any
answer containing "reached through", "driven in a browser", "you're signed in",
"no api key" or "handshake".

### A reply cut in half, and how the window was chosen

The page held the answer; the relay returned the first 208 characters and closed
the turn. From the page itself, on the same conversation:

```
[inspect] last assistant text (992 chars): Here is the answer.
          DeepChatCode is a local, OpenAI-compatible bridge ...
```

The relay had recorded **208**. That is the "it got stuck printing the answer"
symptom: the turn ended mid-sentence, so the client printed what it had and sat
there. The cause was the quiet window — "three identical reads" at 150 ms per
poll is half a second of stillness, and the turn ended on the first pause longer
than that.

How long that window should be was measured, not guessed, by timing the arrival
of every streamed delta (a delta arrives when the page has something new to show,
so the gaps are the page's own rendering gaps):

```
[stream] 7 delta(s), arrival gaps (ms): [0, 164, 0, 165, 0, 3352] max=Some(3352)
[stream] reassembled 264 chars: "The ocean covers most of our planet's surface ..."
```

The ~165 ms gaps are the page rendering. The 3352 ms gap is the new quiet window
itself, between the last word and the closing chunk — the price of the fix, paid
once per turn. Typical gaps are ~20× smaller than the window now, where they used
to be a third of it.

And it is pinned by a test that needs no network and no account: a fixture page
writes half an answer, pauses longer than the old window, then writes the rest.
Against the old value it fails in exactly the way the user described —

```
deepchatcode: reply settled after 3 polls (39 chars, 0s); page send control(s): 1
the relay stopped reading at the pause and returned a partial answer:
  "{\"type\":\"final\",\"content\":\"first half\"}"
```

— and against the shipped value it waits and returns the whole answer. The old
value was briefly restored to run that comparison, then put back; the test is
`a_reply_that_pauses_midway_is_still_read_whole`.

A better signal would be the page saying it is still generating. It was checked
rather than assumed, and it does not exist here: at the moment the reply settled,
with the page idle and the answer complete,

```
deepchatcode: reply settled after 20 polls (264 chars, 4s); page send control(s): 0
```

the configured `[selectors] send` matches **nothing** — on this page it never
matches, which is also why submitting has always fallen through to pressing
Enter. So the quiet window is the honest mechanism available, and
`[timeouts] settle_polls` is the knob.

### The relay is the thing that engages pro

```
$ cargo test -- --ignored --nocapture live_relay_engages_pro_for_a_pro_request
[relay] chips before: "DeepThink=off, Search=on"
[relay] pro reply="ELEVEN" model=String("deepseek-pro")
[relay] chips after: "DeepThink=on, Search=on"
[relay] chat reply="TWELVE"
[relay] chips after the chat request: "DeepThink=off, Search=on"
test result: ok. 1 passed; 0 failed; finished in 8.56s
```

A real POST to the real router, with a real browser on a copy of the profile: the
state changes are the relay's doing, and the second request proves the first did
not leak. This test is also what caught the model field being hardcoded — see
[Fixed along the way](#fixed-along-the-way).

### Attach mode

```
[live attach] attached to https://chat.deepseek.com/
[live attach] reply="PONG"
```

### Local OCR and vision

```
DeepChatCode OCR self-test        # tesseract, ollama and auto all agreed
Invoice 2024-0042
Total: 137.50 USD
handshake-verified
```

Vision, describing three frames of the demo clip: *"…the specific instruction is
shown… The complete AI response follows: 'A browser relay is a technique where a
server or script controls a real web browser…' State of Answer: the answer is
complete."*

### Remote desktop

```
# grdctl status
Overall: Unit status: active
RDP:  Status: enabled   Port: 3389   View-only: no   Username/Password: (hidden)

# ss -ltn
LISTEN 0 5 *:3389 *:*

# X.224 probe
127.0.0.1:3389 -> TPKT len=19 X.224 Connection Confirm
    RDP NEG_RSP selected-protocol=CredSSP(NLA)

# xfreerdp 3.32.1, +auth-only
correct password -> exit 0, no ERRCONNECT_LOGON_FAILURE, server logs no auth failure
wrong password   -> exit 134, ERRCONNECT_LOGON_FAILURE [0x00020014],
                    server logs "client authentication failure"
```

## Gaps — what is *not* verified

1. **`[transport] mode = "api"` does not work against DeepSeek, and cannot be
   made to without reimplementing an anti-bot control.** This is a finding, not
   an untested path. The evidence, captured from live traffic with
   `cargo test -- --ignored live_discover_api_endpoint`:

   - the endpoint is `POST https://chat.deepseek.com/api/v0/chat/completion`
     (the shipped default URL was right);
   - the body is the site's own shape, not OpenAI's —
     `{"chat_session_id":…,"parent_message_id":null,"model_type":"default",
     "prompt":…,"ref_file_ids":[],"thinking_enabled":false,"search_enabled":true,
     "action":null,"preempt":false}`;
   - the request carries `authorization: Bearer <token>` **plus** three headers
     the page derives per request: `x-ds-pow-response` (a proof-of-work from
     `POST /api/v0/chat/create_pow_challenge`), `x-hif-dliq`, and `x-hif-leim`;
   - fetching without the token returns `{"code":40003,"msg":"INVALID_TOKEN"}`;
     with the page's token, `{"code":40300,"msg":"MISSING_HEADER"}`.

   So the site's own page solves a per-request challenge, which is exactly what
   `gui` mode drives. Synthesizing that proof-of-work is deliberately outside
   this project. The `api` mechanism itself is complete (configurable URL, body
   template, framing, text path, static headers) and unit-tested; it is left in
   place for endpoints that need no such header.

2. **No graphical RDP session was rendered to a client.** Authentication is
   verified end to end (correct password accepted, wrong password rejected by the
   server), but pixels arriving and input landing are not, because that needs a
   live client session and this host's desktop is in use. One
   `xfreerdp /v:<your-lan-ip> /u:<user> /cert:ignore` from your client closes
   this.

3. **Model identity is what the page displays**, not an API fact. The chips
   (`DeepThink`, `Search`) are read from the DOM; if the page changes, it records
   `unknown` until `[selectors] model_label` is updated.

4. **`allow_extra`** (accepting a tool call the request never declared) is
   unit-tested but was not exercised against the live model.

5. **Refusal/regression on a changed chat UI** is a known class of failure:
   selectors are config so it can be patched without a rebuild, but no test
   detects a UI change for you.

6. **Two byte-identical replies in a row, with the mounted window already full,
   are indistinguishable from no reply.** Detection compares the newest reply's
   text *and* the element count; if both are unchanged there is nothing to see,
   and the turn waits out `response_secs` and is recorded as `page_silent`. Not
   observed in practice — a Codewhale turn's prompt differs every time, so its
   answer does too — but it is the residual hole in this design. Closing it
   properly means detecting the page's own "generating" state (the stop control)
   instead of comparing transcripts; the selectors for that are not verified.

7. **Codewhale's own footer still says `cost: unknown (billing basis unknown)`
   for this route, and the wrapper cannot change that.** This was measured, not
   assumed, by driving real `codewhale` against a mock provider and reading its
   rendered footer through a pty. Five shapes of provider-side answer were tried
   and none moved it:

   - a per-model `"pricing": "Unlimited Chat!"` string
   - numeric `input_per_million` / `output_per_million`
   - an OpenRouter-style `pricing: {prompt, completion}` with `context_length`
   - response-level `endpoint_fingerprint` / `fetched_at` / `catalog_fetched_at`
   - a provider-level `billing_mode = "not_money_metered"` key in Codewhale's own
     config
   - a per-provider `custom_models` entry carrying `pricing = "Unlimited Chat!"`
     (this one is worth naming: `custom_models` is a *provider* key, not a
     top-level one, which is why earlier probes seemed to accept and reject the
     same shapes)

   The control that makes the mechanism visible is the same endpoint declared as
   `ollama`: the footer then shows **no cost at all** (`Ollama · deepseek-chat ·
   ctx 91%`), because Codewhale knows that route is not money-metered. So the
   value comes from Codewhale's catalog and its own billing-basis classification,
   not from anything the provider returns over the OpenAI-compatible surface.
   The wrapper's part is done and tested; the footer is Codewhale's to decide.
   Two levers exist on this side if the footer matters more than the label:
   Codewhale's undocumented `custom_models` config map (every shape tried parsed,
   so the parser cannot be used to infer its meaning — it needs its docs), and
   launching Codewhale under a provider kind it treats as non-metered, which is
   verified against a mock but not yet against the live bridge.

## Environment

- Bluefin 44 (Fedora Silverblue, atomic), GNOME on Wayland, no sudo
  (no-new-privs), 24 cores, 62 GB RAM, Intel UHD 770 (no discrete GPU).
- Playwright Chromium 1243 via `playwright-rs` 0.19; profile at
  `~/.codewhale/deepseek-chat/browser`.
- `codewhale 0.10.0` at `~/.cargo/bin/codewhale`.
- Local models via Ollama: `gemma4:26b` (vision, CPU-only) and
  `qwen2.5-coder:14b`.
- tesseract 5.5.3 (Linuxbrew), languages `eng`, `osd`.
- DeepSeek account: signed in; **Search on by default, DeepThink off** during
  these runs.
- FreeRDP 3.32.1 (Linuxbrew) added for the RDP authentication check.

## Fixed along the way

- `[browser] headless` was ignored — every run launched a visible window.
- `--cdp-endpoint` did not imply attach mode; it silently tried to launch the
  already-in-use profile instead. A CDP endpoint now always means attach.
- A killed run left an orphaned Chromium holding the profile; the next launch
  died with a 30-line node stack trace. The wrapper now detects the holder and
  names it in one sentence.
- Every fresh turn hunted for a "New chat" button and burned 5 s when the click
  did not land. Returning to the bare chat URL is both correct and instant;
  this took the turn from 6.3 s to 1.4 s.
- Reading the whole assistant transcript on every poll made long conversations
  slow, so detection was narrowed to *counting* replies and reading the newest
  one. The count then *caused* the next bug — see the next entry — so the newest
  reply's text is what carries the decision now.
- **"Mysterious silence around the 3rd–4th prompt" was the wrapper, not
  DeepSeek.** The chat page virtualizes its transcript: once the mounted window
  is full it unmounts an old message in the same render it mounts the new one,
  so `count > previous` stopped being true even though a reply had arrived.
  Every stalled turn was recorded `FAILED (page_silent)` while the page had
  already answered — the answer to a turn declared silent was still sitting on
  the page when it was inspected afterwards, in full, and a *fresh* conversation
  always worked, which is what pointed at the conversation's length rather than
  at the service. Detection now compares the newest reply's text as well as the
  count. Live proof: six turns in one conversation, replies `W1`…`W6` in ~1.9 s
  each, mounted count `3 -> 3` on turns 4, 5 and 6. Nothing about the timeouts or
  the prompt size was the problem, and raising `response_secs` would only have
  hidden it.
- **Almost shipped a silent lie**: the pro work was done for the page path and
  left undone for the api path, which is exactly the path `silent` uses. A
  `deepseek-pro` request over a working api endpoint would have been answered
  without any reasoning flag, and nothing anywhere would have said so — the reply
  would have come back labelled `deepseek-pro`. Found by an independent reviewer
  reading for the invariant ("can a pro request end up answered by the plain
  model without failing?") rather than by the tests, which passed. Fixed by
  teaching the template a `{thinking}` placeholder and refusing when the page is
  in pro mode and the template cannot express it, so the turn falls back to the
  headless page instead. Verified live.
- **An undeclared tool name could kill a whole agent run.** Found by the real
  agent test: a request declared no tools, the model called one anyway, and the
  wrapper's declared-name check answered 502 — Codewhale exited 1 and the run was
  over. The wrapper now enforces the wire *shape* and carries the call; Codewhale
  judges tool names, as the owner of the tool catalog. The old test that pinned
  refusal was rewritten to pin the new contract, with the reason next to it.
- **Cold start put the browser in front of the TUI.** The wrapper opened the page
  before starting Codewhale, on purpose, so a stale link would surface early. The
  cost was the whole browser bring-up in front of the terminal the user is
  watching. Codewhale now starts first and the browser warms in parallel, and
  navigation waits for `DOMContentLoaded` rather than full `load`. Measured:
  browser ready 1.8-2.2 s headless (was 2.7-6.6 s), 5.5 s visible (was 10.1 s),
  Codewhale at 0.00 s.
- **The tool call could arrive and be thrown away.** A reply that put the
  envelope after the model's own reasoning was parsed as prose, so the JSON was
  printed at the user and the tool never ran. Found by running the user's reported
  message verbatim, three times, rather than by a test whose prompt was written to
  pass. The envelope is now found wherever it sits, fenced or trailing, with the
  prose kept as content.
- **The bridge could stall the agent, and a change of mine caused it.** Bridging
  Codewhale's briefing made the model treat the setup as native tool calling, so
  it answered "Let me look." and stopped: prose is a final answer here, so the
  loop ended with nothing done and no error. The delivery contract is now
  explicit, marked as not-style, and repeated after the payload, with two named
  rules — never announce an action, never claim a tool ran — and a live test that
  asks for a repo check and requires `finish_reason: tool_calls`.
- **The instruction text described the wrapper to the model, and the model
  recited it back.** `assets/system-prompt.md` carried a paragraph about being
  reached through the web page; a real session answered the user's question by
  repeating it, and the briefing Codewhale actually wanted the model to have was
  dropped. The paragraph is gone — the file is now only the transport contract
  (prose, or a tool-calls object) — and `[relay] forward_system_prompt`.
  default is now `true`, so the model gets Codewhale's own project briefing,
  constitution and skills index. Bridging the real context beats describing it.
- **A reply could be cut in half and returned as complete.** The quiet window
  that ends a turn was three polls of 150 ms — half a second — so any render pause
  longer than that ended the turn mid-sentence. Found from a user report of a turn
  "stuck printing the answer": the page held a **992-character** answer that the
  relay had recorded as **208**. The window is now ~3 s, chosen against measured
  page gaps (~165 ms, so a ~20× margin where it was a 1/3 margin), and it costs a
  few seconds once per turn — measured, and stated rather than hidden. Whether a
  page-level "still generating" signal exists was also checked: it does not; the
  configured `send` selector matches nothing on this page, idle or busy.
- Answering a `deepseek-pro` request worked, but the response said
  `"model": "deepseek-chat"`: the model field was hardcoded in both the streamed
  chunk writer and the plain completion response. The page had genuinely been put
  into pro mode, so the answer was right and the *label* was wrong — which is the
  same class of error as answering with the wrong model, and just as misleading in
  a log. The requested model is now carried through to every response, asserted
  offline and read back live (`model=String("deepseek-pro")`). Found by writing
  the relay-level test that reads the page, not by reading the code.
- The page's DeepThink control was configured (`[selectors] thinking_toggle`) and
  **never clicked**. The wrapper read the mode chips to *attribute* a turn, but
  never set one, so a "pro" model could not have existed however it was named.
  `deepseek-pro` now drives that control: clicked only when the state differs,
  read back so a click that did not land is a failure rather than a silent lie,
  and put back for the next chat turn. Live chips: `off → on → on through a real
  turn → off`.
- The `api` transport's refusal killed the whole turn, which would have made
  `silent` useless on the day it was added. A refusal now falls back to the
  headless page for that turn, with one line on stderr saying so, because the
  page is already open and already invisible. `silent` stays quiet either way.
- The instruction text never said what the model was attached to, so asked "do
  you cost me deepseek api credits?" the model answered "Yes — each turn you
  send here consumes DeepSeek API credits": it has no view of the user's account
  and guessed. `assets/system-prompt.md` now states that it is reached through
  the chat **web page**, not the API, and to say it cannot see billing rather
  than invent it.
- An `api` failure said "returned no text (48 bytes)" with no clue why. It now
  reports the HTTP status and the first 240 bytes of the body, which is what
  made the `INVALID_TOKEN` → `MISSING_HEADER` diagnosis possible.
- `install-deepseek-browser` shelled out to a `playwright` CLI that usually is
  not on `PATH` (it is an npm package) and panicked with `No such file or
  directory`. It now uses the crate's own installer, so the browser always
  matches the driver — and a first run installs it automatically instead of
  requiring that command at all.
- Bare `cargo run` refused to run because the crate has two binaries. `default-run`
  now points at the bridge.
- A browser that was closed or crashed mid-session cost the whole turn: the relay
  only found out when `page.locator(…)` failed, or after the 300 s reply timeout,
  and then started a *fresh* chat. It now watches the page and context for closure,
  reopens on the conversation in progress, and retries the turn once.
- Closing an already-dead context printed a 7-line node stack trace (`TargetClosedError`)
  on the recovery path. Dead is expected there; it is quiet now.
- The turn log recorded only *successful* turns: the 14:36 turn that got no reply
  and timed out at 14:41 left no row at all, so the log could not answer "what
  went wrong?". Failures are now recorded with a diagnosis, and a `blame` field
  says whether it was the network, the wrapper, or the service.
- A Playwright failure printed its whole node stack trace at the user. The message
  is now one line; the full text stays in the audit log the wrapper already names.
- `ALTER TABLE`-less schema drift: `CREATE TABLE IF NOT EXISTS` never adds columns,
  so an existing `sessions.db` would have silently dropped the new failure fields.
  Opening the database now migrates it (verified against a copy of the live one:
  16 rows preserved).
- A browser that was closed or crashed while the bridge sat **idle** stayed gone
  until the next prompt. `[browser] liveness_check_secs` (default 15 s) now reopens
  it on the conversation in progress, with backoff, so "it was closed" heals by
  itself.
- Three live tests used the *real* browser profile, so they hung in the sign-in
  wait whenever a session held it (one did exactly that here). They now run on a
  copy, which makes them repeatable and unable to disturb a session in use.
