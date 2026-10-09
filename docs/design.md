# The split

FreeChatCode is one piece of a three-part system, and every design question here
resolves the same way: **who owns what**. Getting this boundary wrong is what
produced the two worst bugs in this repository's history, so it is written down
rather than implied.

## The three owners

**DeepSeek Chat (the page) owns the conversation.** The thread — every user
message, every answer, every tool result — lives in the web session. That is not
a limitation to work around; it is the resource this project exists to use. It is
also why a long conversation costs nothing to continue and why a resumed session
picks up where it left off.

**Codewhale (the harness) owns the loop.** It decides when to call a tool, runs
it in the workspace, feeds the result back, keeps going until it is done, and
holds the permissions, approvals, and tool catalog. None of that is the
wrapper's business, and the wrapper must never try to do it.

**FreeChatCode (the wrapper) owns the move.** It is a *model endpoint*: messages
go in, text comes out. Nothing more.

```
Codewhale ── requests ──▶ wrapper ── text ──▶ the page
    ▲                                            │
    └────────── text, or tool calls ◀────────────┘
```

## What the wrapper guarantees

- **One request, one completion.** A request is answered from the page's
  conversation and nothing else.
- **No transcript management.** The page holds the thread; the wrapper sends the
  *delta* Codewhale added since the last assistant message, and feeds the whole
  transcript only when the conversation is new or the link is stale. It keeps no
  copy of the conversation.
- **A wire adaptation, not a rewrite.** A chat page cannot emit native tool
  calls, so a reply is read as either prose or a `tool_calls` object — found
  wherever the model wrote it, fenced or trailing or after its own reasoning —
  and normalised to the OpenAI shape. The model's words are otherwise carried
  through unchanged.
- **The wire *shape* is enforced; the tool *set* is not.** A call with no
  function name, or arguments that are not a JSON object, is refused: no harness
  can act on it. A call naming a tool the request did not declare is **carried**,
  because Codewhale is the authority on which tools exist and answers the model
  itself when one does not. Policing names here turned a model's guess into a 502
  that killed a whole agent run — see `STATUS.md`.
- **Model identity is page state.** `deepseek-pro` is `deepseek-chat` with the
  page's DeepThink control engaged; the wrapper sets that state before a turn,
  reads it back, and reports what the page showed.
- **The terminal belongs to Codewhale, and the wrapper keeps no claim on it.**
  It is handed over at 0.00s and the wrapper's own stdin is pointed at
  `/dev/null` before the browser exists. This is not tidiness: the browser's
  driver library snapshots *stdin's* line discipline at launch and writes that
  snapshot back when it shuts down, so a wrapper that holds the terminal can
  hand it back cooked and echoing while the TUI believes it is raw — and the
  TUI's own mouse reports are then painted over the screen as text. Measured,
  root-caused and fixed; see `STATUS.md`.

## What the wrapper deliberately does not do

- **It does not decide the loop.** Prose ends the turn *because the contract says
  prose is a final answer*, not because the wrapper judged the work finished.
- **It does not lecture the model about itself.** No paragraph explaining how the
  session is reached: a real session answered the user by reciting exactly that
  paragraph instead of doing the work.
- **It does not rewrite or second-guess the model's answer.** There is no
  phrasing heuristic that decides "this reply sounds like a promise, retry it".
  That would make the wrapper a participant, and a participant's guesses are
  indistinguishable — to the user — from the model's.
- **It does not keep conversation state of its own** beyond the session link and
  the browser page it drives.

## Where resume lives

The wrapper links a conversation to a **Codewhale session**. That works whenever
there is one: an interactive `freechatcode launch codewhale` persists a session, so
the next run navigates back to the linked conversation instead of re-feeding the
transcript.

A one-shot `codewhale exec` saves **no** session — `--session-id` resumes an
existing session, it does not create one (measured: 47 session files before a run,
47 after) — so two independent one-shot runs cannot recall each other. The honest
behaviour there is to start clean, and that is what happens; a run that
"remembered" without a session would be inventing continuity. Verified live by
`live_a_one_shot_run_leaves_no_session_to_inherit`: the tool's file is on disk, no
session appears, and the next run says it does not know rather than guessing.

## What this buys, and what it costs

Bought: one turn is a plain request/response, so anything that speaks the
OpenAI dialect can be pointed at it; the thread is the page's, so a long
conversation is free to continue; and the wrapper has no opinion to be wrong
about.

Cost, stated plainly: when the model writes "Let me look." and marks it as its
final answer, the turn ends. That is a model behaviour the wrapper carries
rather than corrects. The contract tells the model not to (see
`assets/system-prompt.md`), and that is the whole of the mechanism — a heuristic
here would be the wrapper inventing intent.

## Evidence

Verified live, with nothing mocked — the shipped wrapper binary, real Codewhale,
a real browser on a copy of the profile. Three tests, each a different property of
the split:

```
$ cargo test --locked --bin freechatcode -- --ignored --nocapture

live_a_real_agent_reads_a_file_through_the_bridge
[agent] exit=Some(0) | tool read completed: The token is MAGIC-TOKEN-7F3A.
finished in 13.40s / 13.64s (run twice)

live_a_two_step_tool_loop_survives_the_round_trip
[loop] exit=Some(0) tools_reported=2 stdout="…LOOP-TOKEN-3B58"
finished in 19.89s

live_a_one_shot_run_leaves_no_session_to_inherit
[one-shot] file on disk: "THREAD-TOKEN-9C41"
[one-shot] codewhale sessions before=47 after=47
[one-shot] second run: "I do not know. … no record of prior actions in this session."
finished in 21.52s
```

The token in the first two exists only inside the file the agent read, so nothing
passes unless a tool ran, its result went back into the page's thread, and the
answer came out of that same thread. The loop, the tools and the parsing were all
Codewhale's; the wrapper carried messages in and text out.
