//! DeepChatCode — DeepSeek Chat browser relay for Codewhale.
//!
//! This crate exposes a short-lived loopback OpenAI-compatible endpoint and
//! relays each completion through the visible DeepSeek Chat page with
//! Playwright-RS. Codewhale owns the turn, tools, permissions, approvals and
//! workspace; this process is only the bridge.
//!
//! Known limits: the consumer UI is not an API contract; selectors can change,
//! account-side availability and model identity are not reported as API facts,
//! and only one browser conversation is active at a time. When Codewhale
//! switches or compacts its transcript, the relay starts a fresh UI
//! conversation and resends the full current context. Usage and cost are
//! unknown. This does not promise that browser use is permitted by any
//! particular account plan or that it is cheaper than the supported API.
//!
//! Images and files are bridged by extracting their content and uploading
//! through the visible browser's file input. Tool calls, hooks, and model
//! settings (search, deep thinking, temperature, max tokens) are detected
//! from the request and bridged to the web UI without hardcoding.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const MODEL_ID: &str = "deepseek-chat";

/// The reasoning model. Same page, same session — it is `deepseek-chat` with the
/// page's DeepThink mode engaged, which is how the site itself exposes a
/// stronger model. Nothing hardcoded beyond the name: the toggle is config.
pub const PRO_MODEL_ID: &str = "deepseek-pro";

/// The one answer to "what does this cost?".
///
/// A string, and not a rate, because the honest answer on this route is not a
/// rate: the work is done by a Chat page the user is already signed in to, so
/// there is no per-token bill to report. Everything the wrapper is asked about
/// pricing returns this, so a caller never has to render "unknown".
pub const PRICING_LABEL: &str = "Unlimited Chat!";

/// Whether this wrapper serves `id`.
#[must_use]
pub fn serves_model(id: &str) -> bool {
    id == MODEL_ID || id == PRO_MODEL_ID
}
/// The instruction text handed to the chat model. Committed and embedded in the
/// binary; override the path with `[codewhale] system_prompt` in config.
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../assets/system-prompt.md");
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 4 * 1024 * 1024;
/// How often a keep-alive comment is written while the browser turn runs.
const KEEPALIVE: Duration = Duration::from_secs(15);

pub mod config;
pub mod health;
pub mod sessions;
pub mod setup;

#[async_trait]
pub trait ChatUi: Send + Sync {
    async fn send(&self, prompt: &str, start_new_chat: bool) -> Result<String, String>;

    /// Like [`ChatUi::send`], but report the visible reply as it grows so the
    /// caller can stream it. Each message is the *whole* visible text so far,
    /// not an incremental delta, so a lost or reordered message cannot corrupt
    /// the stream. The default implementation does not stream.
    async fn send_streaming(
        &self,
        prompt: &str,
        start_new_chat: bool,
        snapshots: tokio::sync::mpsc::Sender<String>,
    ) -> Result<String, String> {
        let reply = self.send(prompt, start_new_chat).await?;
        let _ = snapshots.send(reply.clone()).await;
        Ok(reply)
    }

    /// The model the page says it used for the last turn, when the UI exposes
    /// it. Best-effort: `None` is a normal answer.
    async fn model_label(&self) -> Option<String> {
        None
    }

    /// Put the page into the reasoning mode the next turn needs.
    ///
    /// `deepseek-pro` is not a second endpoint: it is this page with its
    /// DeepThink control engaged, so the model that answers is decided by page
    /// state. Called before every turn with the mode that turn asked for, which
    /// is also how the state is put back. Implementations that cannot reach the
    /// control must fail: answering a pro request with the plain model while
    /// claiming otherwise is worse than not answering.
    async fn set_reasoning(&self, pro: bool) -> Result<(), String> {
        let _ = pro;
        Ok(())
    }

    /// Describe a failed turn: what went wrong, and whose fault it is. This is
    /// where a transport can tell a dead network from a service that answered
    /// badly. The default claims nothing.
    async fn diagnose(&self, error: &str) -> Option<Failure> {
        let _ = error;
        None
    }
}

#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn append(&self, record: Value) -> Result<(), String>;
}

/// Why a turn produced no answer, and whose fault that is.
///
/// The distinction matters: a DNS failure or a dead uplink is not the chat
/// service misbehaving, and blaming it for those makes the log useless.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// Machine-readable kind: `dns`, `network`, `page_silent`, `browser_gone`,
    /// `request`, `upstream_http`, `unknown`.
    pub kind: String,
    /// Who is to blame: `network`, `wrapper`, `service`, or `unknown`.
    pub blame: String,
    /// What was actually observed.
    pub detail: String,
    /// The HTTP status, when the service demonstrably answered.
    pub http_status: Option<u16>,
}

impl Failure {
    /// A failure the relay caused itself: a bad request, a bad tool call, bad
    /// configuration. The network and the service are not involved.
    #[must_use]
    pub fn request(detail: impl Into<String>) -> Self {
        Self {
            kind: "request".to_owned(),
            blame: "wrapper".to_owned(),
            detail: detail.into(),
            http_status: None,
        }
    }

    /// A failure with no established cause.
    #[must_use]
    pub fn unknown(detail: impl Into<String>) -> Self {
        Self {
            kind: "unknown".to_owned(),
            blame: "unknown".to_owned(),
            detail: detail.into(),
            http_status: None,
        }
    }

    /// Whether the service is demonstrably at fault: it answered, with a status.
    #[must_use]
    pub fn is_service_fault(&self) -> bool {
        self.blame == "service"
    }
}

/// What the relay knows about one finished turn — answered or not. Recorded
/// through a [`TurnSink`] so a conversation can be attributed model-by-model
/// after the fact, and so a failure is a record rather than a silence.
#[derive(Clone, Debug, Default)]
pub struct TurnRecord {
    /// The model label the page showed for this turn, when it exposes one.
    pub model_label: Option<String>,
    /// `stop`, `tool_calls`, or `error`.
    pub finish_reason: String,
    /// Whether the turn asked for tools rather than answering.
    pub tool_calls: bool,
    /// Characters of assistant text in the parsed message.
    pub content_chars: usize,
    /// Set when the turn produced no answer.
    pub failure: Option<Failure>,
}

#[async_trait]
pub trait TurnSink: Send + Sync {
    async fn record(&self, turn: TurnRecord) -> Result<(), String>;
}

/// A sink that drops every turn. The default for callers that do not keep a
/// turn log.
#[derive(Debug, Default)]
pub struct NoopTurns;

#[async_trait]
impl TurnSink for NoopTurns {
    async fn record(&self, _turn: TurnRecord) -> Result<(), String> {
        Ok(())
    }
}

/// Which tool definitions are forwarded to the chat, and which tool-call names
/// are accepted. Populated from `[tools]` in config; the only built-in default
/// is the `tool_search` discovery name.
#[derive(Clone, Debug)]
pub struct ToolPolicy {
    /// Forward Codewhale's full catalog. When false, only `essential` + `search`.
    pub forward_all: bool,
    /// Tool names always forwarded, even when `forward_all` is false.
    pub essential: Vec<String>,
    /// Discovery/search tool names, always forwarded.
    pub search: Vec<String>,
    /// Extra tool-call names to accept even if not declared in the request.
    pub allow_extra: Vec<String>,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            forward_all: true,
            essential: Vec::new(),
            search: vec!["tool_search".to_owned()],
            allow_extra: Vec::new(),
        }
    }
}

impl ToolPolicy {
    fn always(&self, name: &str) -> bool {
        self.essential.iter().any(|candidate| candidate == name)
            || self.search.iter().any(|candidate| candidate == name)
    }

    /// The subset of `tools` to embed in the browser prompt.
    fn forwarded(&self, tools: &[Value]) -> Vec<Value> {
        if self.forward_all {
            return tools.to_vec();
        }
        tools
            .iter()
            .filter(|tool| {
                tool.pointer("/function/name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.always(name))
            })
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct BridgeOptions {
    pub tools: ToolPolicy,
    /// Ask the browser for a brand-new conversation on the first turn.
    pub start_fresh: bool,
    /// Instruction text sent ahead of each request. `{payload}` marks where the
    /// Codewhale request JSON goes.
    pub system_prompt: Arc<str>,
    /// Forward Codewhale's own system message. Off by default: it carries the
    /// whole project briefing, which the instruction text replaces.
    pub forward_system_prompt: bool,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            tools: ToolPolicy::default(),
            start_fresh: false,
            system_prompt: Arc::from(DEFAULT_SYSTEM_PROMPT),
            forward_system_prompt: true,
        }
    }
}

#[derive(Clone)]
pub struct ServerState {
    token: Arc<str>,
    relay: Arc<Mutex<ConversationRelay>>,
    ui: Arc<dyn ChatUi>,
    audit: Arc<dyn AuditSink>,
    turns: Arc<dyn TurnSink>,
    options: BridgeOptions,
}

impl ServerState {
    #[must_use]
    pub fn new(token: impl Into<Arc<str>>, ui: Arc<dyn ChatUi>, audit: Arc<dyn AuditSink>) -> Self {
        Self::with_options(token, ui, audit, BridgeOptions::default())
    }

    #[must_use]
    pub fn with_options(
        token: impl Into<Arc<str>>,
        ui: Arc<dyn ChatUi>,
        audit: Arc<dyn AuditSink>,
        options: BridgeOptions,
    ) -> Self {
        Self {
            token: token.into(),
            relay: Arc::new(Mutex::new(ConversationRelay::new(options.start_fresh))),
            ui,
            audit,
            turns: Arc::new(NoopTurns),
            options,
        }
    }

    /// Attach a sink that records a row per finished turn.
    #[must_use]
    pub fn with_turns(mut self, turns: Arc<dyn TurnSink>) -> Self {
        self.turns = turns;
        self
    }

    /// Record a turn, complaining on stderr rather than failing the request when
    /// the log is unwritable.
    async fn record(&self, turn: TurnRecord) {
        if let Err(error) = self.turns.record(turn).await {
            eprintln!("deepchatcode: could not record the turn: {error}");
        }
    }

    /// Record a turn that produced no answer, and why.
    async fn record_failure(&self, failure: Failure) {
        self.record(TurnRecord {
            finish_reason: "error".to_owned(),
            failure: Some(failure),
            ..TurnRecord::default()
        })
        .await;
    }
}

pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(completions))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn models(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    // Both entries are the same page; `deepseek-pro` is the chat with the page's
    // reasoning mode engaged. What a model costs is answered here, in the one
    // place a caller asks, and it is always the same non-empty string.
    //
    // Deliberately absent: `context_length` and `max_output`. The page's real
    // limits are not knowable from the outside, and a made-up number would be a
    // claim this wrapper cannot support.
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let data: Vec<Value> = [MODEL_ID, PRO_MODEL_ID]
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "owned_by": "deepseek-chat-web",
                "pricing": PRICING_LABEL,
                // Numeric companions, for a caller that only understands a rate.
                // Zero, because on this route there is no per-token bill.
                "input_per_million": 0,
                "output_per_million": 0,
                "cache_read_per_million": 0,
                "cache_write_per_million": 0,
            })
        })
        .collect();
    Json(json!({
        "object": "list",
        "data": data,
        "fetched_at": fetched_at,
    }))
    .into_response()
}

async fn completions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<CompletionRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    if !serves_model(&request.model) {
        return api_error(StatusCode::BAD_REQUEST, "unknown wrapper model");
    }
    if let Err(message) = validate_request(&request) {
        // A malformed request never reaches the browser, but it is still a turn
        // that failed, and the log should say so.
        state
            .record_failure(Failure::request(message.clone()))
            .await;
        return api_error(StatusCode::BAD_REQUEST, &message);
    }

    if request.stream.unwrap_or(false) {
        // Send SSE headers up front: the browser turn can take minutes, and an
        // OpenAI client aborts when response headers do not arrive in time.
        return streamed_completion(state, request);
    }

    match relay_turn(&state, &request, None).await {
        Ok(assistant) => completion_response(assistant, &request.model),
        Err((status, message)) => api_error(status, &message),
    }
}

/// The `id`/`created` pair shared by every chunk of one streamed reply, plus the
/// model the caller asked for.
///
/// The model is carried, not assumed: `deepseek-pro` and `deepseek-chat` are
/// answered by the same page, and replying `deepseek-chat` to a `deepseek-pro`
/// request would undo the one thing the model id is for.
struct SseWriter {
    id: String,
    created: u64,
    model: String,
}

impl SseWriter {
    fn new(model: impl Into<String>) -> Self {
        Self {
            id: format!("chatcmpl-{}", Uuid::new_v4().simple()),
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs()),
            model: model.into(),
        }
    }

    /// One `data:` chunk carrying `delta`, optionally closing the turn.
    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> String {
        format!(
            "data: {}\n\n",
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
            })
        )
    }
}

/// Turn the visible reply, as it grows in the browser, into content deltas for
/// an OpenAI streamed response.
///
/// The model answers either in plain text or in a JSON envelope such as
/// `{"type":"final","content":"…"}`. A partial envelope is not valid JSON, so
/// the content is decoded from the bytes available so far and the surplus over
/// what was already emitted is returned. A tool-call envelope never streams:
/// its calls are emitted whole when the turn lands.
#[derive(Default)]
struct ContentStreamer {
    emitted: String,
}

impl ContentStreamer {
    fn push(&mut self, raw: &str) -> Option<String> {
        let visible = partial_content(raw);
        if !visible.starts_with(self.emitted.as_str()) {
            // The shape changed under us (an envelope gave way to plain text,
            // say). Re-baseline without re-emitting what the client already has.
            self.emitted = visible;
            return None;
        }
        if visible.len() <= self.emitted.len() {
            return None;
        }
        let delta = visible[self.emitted.len()..].to_owned();
        self.emitted = visible;
        Some(delta)
    }
}

/// Respond with SSE headers immediately, then stream the reply as it is written
/// while writing a keep-alive comment every [`KEEPALIVE`] so a slow turn cannot
/// trip the client's header timeout.
fn streamed_completion(state: ServerState, request: CompletionRequest) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(32);
    tokio::spawn(async move {
        let writer = Arc::new(SseWriter::new(request.model.clone()));
        // A real `data:` chunk with an empty delta. SSE comment lines are
        // ignored by some clients for idle-timer purposes, so this keeps the
        // stream alive without emitting any content.
        let keepalive = Bytes::from(writer.chunk(json!({}), None));

        let (delta_sender, mut delta_receiver) = tokio::sync::mpsc::channel::<String>(64);
        let stream_writer = Arc::clone(&writer);
        let stream_sender = sender.clone();
        // Forwards the growing visible reply as content deltas, and returns the
        // text it emitted so the final chunk never duplicates it.
        let streamer = tokio::spawn(async move {
            let mut streamer = ContentStreamer::default();
            while let Some(snapshot) = delta_receiver.recv().await {
                let Some(delta) = streamer.push(&snapshot) else {
                    continue;
                };
                if stream_sender
                    .send(Ok(Bytes::from(
                        stream_writer.chunk(json!({"role": "assistant"}), None),
                    )))
                    .await
                    .is_err()
                {
                    return streamer.emitted;
                }
                if stream_sender
                    .send(Ok(Bytes::from(
                        stream_writer.chunk(json!({"content": delta}), None),
                    )))
                    .await
                    .is_err()
                {
                    return streamer.emitted;
                }
            }
            streamer.emitted
        });

        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let turn = relay_turn(&state, &request, Some(delta_sender));
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                result = &mut turn => break result,
                _ = ticker.tick() => {
                    if sender.send(Ok(keepalive.clone())).await.is_err() {
                        return;
                    }
                }
            }
        };
        // Every delta has been produced by the time the turn returns, so this
        // join flushes the stream before the closing chunk goes out.
        let streamed = streamer.await.unwrap_or_default();

        let body = match outcome {
            Ok(assistant) => streamed_body(&writer, &assistant, &streamed),
            Err((_, message)) => sse_error(&message),
        };
        let _ = sender.send(Ok(Bytes::from(body))).await;
    });

    let stream = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        )
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .body(Body::from_stream(stream))
        .expect("the streaming response is well formed")
}

/// The closing chunks of a streamed turn: whatever content was not streamed
/// live, any tool calls, the finish reason, and `[DONE]`.
fn streamed_body(writer: &SseWriter, assistant: &Value, streamed: &str) -> String {
    let finish_reason = if assistant.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let mut body = String::new();
    let content = assistant.get("content").and_then(Value::as_str);
    let remaining = match content {
        // Everything the browser showed has already gone out as deltas. If the
        // streamed text diverged from the parsed answer, the client already
        // holds the answer, so an empty remainder avoids a second copy.
        Some(content) if !streamed.is_empty() => content.strip_prefix(streamed).unwrap_or_default(),
        Some(content) => content,
        None => "",
    };
    if !remaining.is_empty() {
        body.push_str(&writer.chunk(json!({"role": "assistant"}), None));
        body.push_str(&writer.chunk(json!({"content": remaining}), None));
    }
    if let Some(calls) = assistant.get("tool_calls") {
        body.push_str(&writer.chunk(json!({"role": "assistant"}), None));
        body.push_str(&writer.chunk(json!({"tool_calls": calls}), None));
    }
    body.push_str(&writer.chunk(json!({}), Some(finish_reason)));
    body.push_str("data: [DONE]\n\n");
    body
}

/// The marker the delivery contract requires at the start of a final answer.
const FINAL_ANSWER_MARKER: &str = "Here is the answer.";

/// The nudge sent when a reply arrived in neither required form.
const PROTOCOL_REPAIR: &str = "Your previous reply was neither the tool_calls JSON \
object nor a final answer starting with \"Here is the answer.\", so it was not \
delivered and the turn stopped. Reply again, in one of those two forms only — if \
you meant to use a tool, reply with the JSON object now.";

/// Whether a reply failed the delivery contract badly enough to re-ask.
///
/// Only on a turn that supplied tools (without them prose *is* the answer), and
/// only when the reply carries neither a tool-call envelope nor the final-answer
/// marker. Anything that is one or the other is accepted, however short: the
/// point is not to police style, it is to stop a turn from ending on a sentence
/// like "Let me look."
fn needs_protocol_retry(raw: &str, tools: Option<&[Value]>) -> bool {
    let supplied_tools = tools.is_some_and(|tools| !tools.is_empty());
    if !supplied_tools {
        return false;
    }
    let text = raw.trim();
    if text.is_empty() || text.starts_with('{') || text.starts_with("```") {
        // An envelope attempt, well-formed or not — the parser's problem, not
        // something a second ask fixes.
        return false;
    }
    !text.starts_with(FINAL_ANSWER_MARKER)
}

/// Run one relay turn end to end: replay, prepare, drive the browser, and parse
/// the reply. Errors carry the HTTP status the non-streaming path should use.
/// When `snapshots` is set, the visible reply is reported as it grows.
async fn relay_turn(
    state: &ServerState,
    request: &CompletionRequest,
    snapshots: Option<tokio::sync::mpsc::Sender<String>>,
) -> Result<Value, (StatusCode, String)> {
    let mut relay = state.relay.lock().await;
    if let Some(assistant) = relay.replay(request) {
        return Ok(assistant);
    }
    let (prompt, reset) = match relay.prepare(request, &state.options) {
        Ok(prepared) => prepared,
        Err(message) => {
            // The browser is never reached, but this is still a turn, and the log
            // should say so rather than staying silent.
            state
                .record_failure(Failure::request(message.clone()))
                .await;
            return Err((StatusCode::BAD_REQUEST, message));
        }
    };
    let request_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    state
        .audit
        .append(json!({
            "kind": "input",
            "request_id": request_id.clone(),
            "model": request.model.clone(),
            "codewhale_messages": request.messages.clone(),
            "tools": request.tools.clone(),
            "browser_prompt": prompt.clone(),
            "new_platform_chat": reset,
        }))
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;

    // The model that answers is decided by the page's own reasoning state, so a
    // pro request is page state, set before the turn starts. A wrapper that
    // cannot set it fails the turn instead of answering as the wrong model.
    if let Err(error) = state.ui.set_reasoning(request.model == PRO_MODEL_ID).await {
        state.record_failure(Failure::request(error.clone())).await;
        return Err((StatusCode::INTERNAL_SERVER_ERROR, error));
    }

    let raw = match match snapshots {
        Some(snapshots) => state.ui.send_streaming(&prompt, reset, snapshots).await,
        None => state.ui.send(&prompt, reset).await,
    } {
        Ok(raw) => {
            // A reply that is neither a tool call nor a marked final answer is the
            // delivery contract failing, not the model answering. On this
            // transport prose IS a final answer, so "I want to check two things
            // ... Let me look." ended Codewhale's loop with nothing done and no
            // error anywhere. Ask once more rather than let a sentence close a
            // turn; whatever the second reply is, it is delivered either way, so
            // this can cost a round trip and never an answer.
            if !needs_protocol_retry(&raw, request.tools.as_deref()) {
                raw
            } else {
                let repair = format!("{prompt}\n\n{PROTOCOL_REPAIR}");
                eprintln!(
                    "deepchatcode: the reply was neither a tool call nor a marked final \
                     answer; asking once more instead of ending the turn on it"
                );
                let _ = state
                    .audit
                    .append(json!({
                        "kind": "retry",
                        "request_id": request_id.clone(),
                        "reason": "reply was neither a tool call nor a marked final answer",
                        "browser_response": raw.clone(),
                    }))
                    .await;
                match state.ui.send(&repair, false).await {
                    Ok(retry) => retry,
                    // The first reply is all there is. Better delivered than lost.
                    Err(_) => raw,
                }
            }
        }
        Err(error) => {
            // Never swallow this: the reason a turn died is otherwise invisible.
            eprintln!("deepchatcode: browser relay failed: {error}");
            // Ask the transport what actually happened before blaming anyone: a
            // dead uplink is not the chat service's fault.
            let failure = match state.ui.diagnose(&error).await {
                Some(failure) => failure,
                None => Failure::unknown(error.clone()),
            };
            let _ = state
                .audit
                .append(json!({
                    "kind": "error",
                    "request_id": request_id,
                    "browser_error": error,
                    "failure": failure,
                }))
                .await;
            state.record_failure(failure.clone()).await;
            return Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "DeepSeek Chat UI did not return a complete response ({error}); {} before retrying",
                    match failure.blame.as_str() {
                        "network" => "this looks like DNS or connectivity, not the service",
                        "service" => "the service itself answered badly",
                        "wrapper" => "this looks like a local problem",
                        _ => "inspect the browser",
                    }
                ),
            ));
        }
    };
    let assistant = relay
        .finish(request, raw.clone(), &state.options)
        .map_err(|message| (StatusCode::BAD_GATEWAY, message))?;
    // Which model the page used is not an API fact; record what the UI showed so
    // a conversation can be attributed model-by-model after the fact.
    let model_label = state.ui.model_label().await;
    let turn = TurnRecord {
        model_label: model_label.clone(),
        finish_reason: if assistant.get("tool_calls").is_some() {
            "tool_calls".to_owned()
        } else {
            "stop".to_owned()
        },
        tool_calls: assistant.get("tool_calls").is_some(),
        content_chars: assistant
            .get("content")
            .and_then(Value::as_str)
            .map_or(0, str::len),
        failure: None,
    };
    state.record(turn).await;
    state
        .audit
        .append(json!({
            "kind": "output",
            "request_id": request_id,
            "browser_response": raw,
            "model_label": model_label,
            "codewhale_assistant_message": assistant.clone(),
        }))
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(assistant)
}

fn completion_response(assistant: Value, model: &str) -> Response {
    let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let finish_reason = if assistant.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    Json(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": assistant, "finish_reason": finish_reason}],
    }))
    .into_response()
}

/// An SSE error body for a turn that failed after headers were already sent.
fn sse_error(message: &str) -> String {
    let encoded = serde_json::to_string(message).unwrap_or_else(|_| "\"relay error\"".to_owned());
    format!(
        "data: {{\"error\":{{\"message\":{encoded},\"type\":\"wrapper_error\"}}}}\n\ndata: [DONE]\n\n"
    )
}

fn authorized(state: &ServerState, headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), state.token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn unauthorized() -> Response {
    api_error(
        StatusCode::UNAUTHORIZED,
        "local wrapper bearer token required",
    )
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {"message": message, "type": "wrapper_error"}
        })),
    )
        .into_response()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: Option<bool>,
}

#[derive(Default)]
struct ConversationRelay {
    /// Whether the next request should begin a new platform conversation.
    start_fresh: bool,
    /// Fingerprint of the conversation head (everything before the first
    /// assistant message). Codewhale rewrites assistant messages but not these.
    head: Option<String>,
    previous_tools: Option<Vec<Value>>,
    last_signature: Option<String>,
    last_message: Option<Value>,
}

impl ConversationRelay {
    fn new(start_fresh: bool) -> Self {
        Self {
            start_fresh,
            ..Self::default()
        }
    }

    fn replay(&self, request: &CompletionRequest) -> Option<Value> {
        let signature = request_signature(request).ok()?;
        (self.last_signature.as_deref() == Some(&signature))
            .then(|| self.last_message.clone())
            .flatten()
    }

    fn prepare(
        &self,
        request: &CompletionRequest,
        options: &BridgeOptions,
    ) -> Result<(String, bool), String> {
        // Reset only when the conversation head changes — not on every tool
        // result. Clicking "New chat" and feeding the whole transcript are
        // separate decisions: a fresh session does both, a resumed one only
        // catches the browser up.
        let head = conversation_head(&request.messages);
        let first_turn = self.head.is_none();
        let reset = if first_turn {
            self.start_fresh
        } else {
            self.head.as_deref() != Some(head.as_str())
        };
        // The first turn of a run always feeds the entire transcript so the
        // browser conversation is aligned with the Codewhale session, whatever
        // state it was left in.
        let full_context = first_turn || reset;
        // On continuation, send only what Codewhale added after its last
        // assistant message (the tool results, or the next user turn) instead of
        // replaying the whole transcript.
        let start = if full_context {
            0
        } else {
            tail_start(&request.messages)
        };
        let delta: Vec<Value> = {
            let window = &request.messages[start..];
            if options.forward_system_prompt {
                window.to_vec()
            } else {
                // Codewhale's system message is the full project briefing. The
                // instruction text already identifies the session, so it is
                // dropped rather than re-explained to the model.
                window
                    .iter()
                    .filter(|message| message.get("role").and_then(Value::as_str) != Some("system"))
                    .cloned()
                    .collect()
            }
        };
        let include_tools = reset || self.previous_tools.as_ref() != request.tools.as_ref();
        // Only the selected subset of Codewhale's catalog is embedded, so the
        // chat prompt does not carry every tool schema.
        let tools = include_tools
            .then_some(request.tools.as_ref())
            .flatten()
            .map(|tools| options.tools.forwarded(tools));
        let tool_choice = include_tools
            .then_some(request.tool_choice.as_ref())
            .flatten();
        let payload = json!({
            "messages": delta,
            "tools": tools,
            "tool_choice": tool_choice,
        })
        .to_string();
        // The instruction text lives in assets/system-prompt.md (or a path from
        // config), never in code. `{payload}` marks where the request lands.
        let prompt = if options.system_prompt.contains("{payload}") {
            options.system_prompt.replace("{payload}", &payload)
        } else {
            format!("{}\n\nCodewhale request:\n{payload}", options.system_prompt)
        };
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err("browser prompt exceeds the 4 MiB relay limit".into());
        }
        Ok((prompt, reset))
    }

    fn finish(
        &mut self,
        request: &CompletionRequest,
        raw: String,
        options: &BridgeOptions,
    ) -> Result<Value, String> {
        let signature = request_signature(request)?;
        let assistant = parse_assistant_message(&raw, request.tools.as_deref(), &options.tools)?;
        self.head = Some(conversation_head(&request.messages));
        self.previous_tools.clone_from(&request.tools);
        self.last_signature = Some(signature);
        self.last_message = Some(assistant.clone());
        Ok(assistant)
    }
}

/// A normalization-proof fingerprint of the start of a conversation: every
/// message before the first assistant message. Codewhale rewrites assistant
/// messages (adds `reasoning_content`, normalises `content`, splits tool calls)
/// but leaves the leading system/user messages intact.
fn conversation_head(messages: &[Value]) -> String {
    let end = messages
        .iter()
        .position(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .unwrap_or(messages.len());
    serde_json::to_string(&messages[..end]).unwrap_or_default()
}

/// Index just after the last assistant message: the tool results or user turns
/// Codewhale appended since the previous request.
fn tail_start(messages: &[Value]) -> usize {
    messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .map_or(0, |index| index + 1)
}

fn request_signature(request: &CompletionRequest) -> Result<String, String> {
    serde_json::to_string(&json!({
        "model": &request.model,
        "messages": &request.messages,
        "tools": &request.tools,
        "tool_choice": &request.tool_choice,
    }))
    .map_err(|error| format!("could not fingerprint chat request: {error}"))
}

fn validate_request(request: &CompletionRequest) -> Result<(), String> {
    if request.messages.is_empty() {
        return Err("chat request must contain at least one message".into());
    }
    if request.messages.len() > 4096 {
        return Err("chat request exceeds the 4096-message relay limit".into());
    }
    for message in &request.messages {
        let Some(object) = message.as_object() else {
            return Err("chat messages must be JSON objects".into());
        };
        let _ = object;
    }
    Ok(())
}

/// Recover the `content` of a `{"type":"final","content":"…"}` envelope that
/// failed to parse — almost always because inner quotes were left unescaped.
fn salvage_final_envelope(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let key = trimmed.find("\"content\"")?;
    if !trimmed[..key].contains("\"type\"") || !trimmed[..key].contains("final") {
        return None;
    }
    let after_key = &trimmed[key + "\"content\"".len()..];
    let body = after_key.trim_start().strip_prefix(':')?.trim_start();
    let body = body.strip_prefix('"')?;
    // Drop the closing `"}` (or a stray brace) of the envelope.
    let end = body.rfind("\"}").or_else(|| body.rfind('}'))?;
    Some(unescape_json_string(&body[..end]))
}

/// Byte length of the balanced JSON object that starts at the first `{` of
/// `text`, ignoring braces inside strings. `None` when they never balance.
fn balanced_object_end(text: &str) -> Option<usize> {
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in text.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Find a tool-call envelope inside a reply that is not *only* the envelope.
///
/// Models put the call after their own reasoning, or in a fenced block, instead
/// of replying with the object alone. Requiring the whole reply to be JSON threw
/// those calls away and printed the JSON at the user as prose. Measured on a live
/// turn, which ended with:
///
/// ```text
/// … Let me read a couple of files.
/// {"type":"tool_calls","tool_calls":[{"id":"call_1", …}]}
/// ```
///
/// The last envelope wins: a model may quote the shape earlier and then act.
/// Returns it with the prose that preceded it, so the caller can keep both —
/// content *and* tool calls is exactly what a native tool-call turn looks like.
fn extract_tool_call_envelope(text: &str) -> Option<(Value, String)> {
    let mut found = None;
    for (index, _) in text.match_indices("\"tool_calls\"") {
        let Some(start) = text[..index].rfind('{') else {
            continue;
        };
        let candidate = &text[start..];
        let Some(end) = balanced_object_end(candidate) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&candidate[..end]) else {
            continue;
        };
        if value.get("tool_calls").is_some_and(Value::is_array) {
            found = Some((value, text[..start].to_owned()));
        }
    }
    found
}

/// Best-effort unescape of the common JSON string escapes.
fn unescape_json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn partial_content(raw: &str) -> String {
    let trimmed = raw.trim_start();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map_or(trimmed, str::trim_start);
    if !trimmed.starts_with('{') {
        return trimmed.to_owned();
    }
    // A tool-call envelope is JSON for the relay, never text for the user. Only
    // a `tool_calls` marker that precedes the content key counts: an answer that
    // merely mentions the word must still stream.
    let content_at = trimmed.find("\"content\"");
    match (trimmed.find("tool_calls"), content_at) {
        (Some(calls), Some(content)) if calls < content => return String::new(),
        (Some(_), None) => return String::new(),
        _ => {}
    }
    let Some(key) = content_at else {
        return String::new();
    };
    let Some(after_key) = trimmed[key + "\"content\"".len()..]
        .trim_start()
        .strip_prefix(':')
    else {
        return String::new();
    };
    let Some(body) = after_key.trim_start().strip_prefix('"') else {
        return String::new();
    };
    let end = find_string_end(body).unwrap_or(body.len());
    let mut body = &body[..end];
    // A truncated escape would decode to a character that changes once the next
    // byte lands, so hold it back until it is complete.
    let trailing = body.len() - body.trim_end_matches('\\').len();
    if trailing % 2 != 0 {
        body = &body[..body.len() - 1];
    }
    unescape_json_string(body)
}

/// Byte index of the first unescaped `"` in `body`, or `None` when the string
/// is still being written. Scanning bytes is safe: UTF-8 continuation bytes are
/// never ASCII, so they cannot be mistaken for `\\` or `"`.
fn find_string_end(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// Extract the assistant text from an `api` transport response body.
///
/// The endpoint is a private one, so its framing is configuration rather than
/// an assumption: `sse` concatenates the text at `text_path` across `data:`
/// lines, `json` reads one document, and `text` takes the body verbatim.
#[must_use]
pub fn extract_api_text(framing: config::ApiFraming, text_path: &str, body: &str) -> String {
    match framing {
        config::ApiFraming::Text => body.to_owned(),
        config::ApiFraming::Json => serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|value| config::text_at_path(&value, text_path))
            .unwrap_or_default(),
        config::ApiFraming::Sse => {
            let mut out = String::new();
            for line in body.lines() {
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(payload) else {
                    continue;
                };
                if let Some(text) = config::text_at_path(&value, text_path) {
                    out.push_str(&text);
                }
            }
            out
        }
    }
}

fn parse_assistant_message(
    raw: &str,
    tools: Option<&[Value]>,
    policy: &ToolPolicy,
) -> Result<Value, String> {
    let cleaned = raw.trim();
    let cleaned = cleaned
        .strip_prefix("```json")
        .and_then(|value| value.strip_suffix("```"))
        .map_or(cleaned, str::trim);
    let Ok(value) = serde_json::from_str::<Value>(cleaned) else {
        // The model frequently emits the final envelope with unescaped quotes,
        // which is not valid JSON. Recover the text instead of showing the
        // wrapper to the user.
        if let Some(content) = salvage_final_envelope(cleaned) {
            return Ok(json!({"role": "assistant", "content": content}));
        }
        // Not envelope-only, but it may still *contain* one, written after the
        // model's reasoning. A call the model did make must not be thrown away
        // and printed as text: the tool never runs and the turn looks stalled.
        if let Some((envelope, prose)) = extract_tool_call_envelope(cleaned) {
            let mut message = parse_assistant_message(&envelope.to_string(), tools, policy)?;
            if message.get("tool_calls").is_some() {
                let prose = prose.trim();
                if !prose.is_empty() {
                    message["content"] = Value::String(prose.to_owned());
                }
                return Ok(message);
            }
        }
        return Ok(json!({"role": "assistant", "content": raw}));
    };
    let action_envelope = value.get("type").and_then(Value::as_str) == Some("tool_calls")
        || value.get("tool_calls").is_some_and(Value::is_array);
    if action_envelope {
        let calls = value
            .get("tool_calls")
            .and_then(Value::as_array)
            .ok_or("tool-call response must contain a tool_calls array")?;
        // The wire shape is enforced here; the *set* of real tools is not. The
        // wrapper carries what the page's model said, and Codewhale is the
        // authority on which tools exist — it answers the model itself when one
        // does not, which is an ordinary recoverable turn. Policing names here
        // turned a model's guess into a 502 that killed the whole agent run, seen
        // live when a request declared no tools and the model called one anyway.
        let _ = (&tools, policy);
        let mut ids = std::collections::BTreeSet::new();
        let mut normalized = Vec::with_capacity(calls.len());
        for call in calls {
            let function = call
                .get("function")
                .ok_or("each tool call must contain a function object")?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or("tool call had no function name")?;
            let arguments = match function.get("arguments") {
                Some(Value::String(serialized)) => {
                    let parsed: Value = serde_json::from_str(serialized)
                        .map_err(|_| "tool arguments were not valid JSON")?;
                    if !parsed.is_object() {
                        return Err("tool arguments must decode to a JSON object".into());
                    }
                    serialized.clone()
                }
                Some(arguments @ Value::Object(_)) => arguments.to_string(),
                _ => return Err("tool arguments must be a JSON object".into()),
            };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control))
                .map_or_else(
                    || format!("call_{}", Uuid::new_v4().simple()),
                    str::to_owned,
                );
            if !ids.insert(id.clone()) {
                return Err("tool call IDs must be unique in one response".into());
            }
            normalized.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }));
        }
        return Ok(json!({"role": "assistant", "content": null, "tool_calls": normalized}));
    }
    if value.get("type").and_then(Value::as_str) == Some("final") {
        if let Some(content) = value.get("content").and_then(Value::as_str) {
            return Ok(json!({"role": "assistant", "content": content}));
        }
        return Err("final response content must be a string".into());
    }
    Ok(json!({"role": "assistant", "content": raw}))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;

    fn install_crypto_provider() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
    }

    #[test]
    fn normalizes_tool_actions_and_carries_a_name_it_was_not_given() {
        let tools = [json!({"type":"function", "function":{"name":"read_file"}})];
        let assistant = parse_assistant_message(
            r#"{"type":"tool_calls","tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":{"path":"src/main.rs"}}}]}"#,
            Some(&tools),
            &ToolPolicy::default(),
        )
        .expect("valid call");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"src/main.rs"}"#
        );

        // A name the request never declared is *carried*, not turned into a
        // failure. The wrapper is a transport: it cannot constrain a chat page's
        // model, and Codewhale is the authority on its own tools — it answers the
        // model itself when a tool does not exist, which is a recoverable turn.
        // Policing it here produced a 502 that killed a whole agent run, observed
        // live when a request declared no tools and the model called one anyway.
        let assistant = parse_assistant_message(
            r#"{"type":"tool_calls","tool_calls":[{"function":{"name":"shell","arguments":{}}}]}"#,
            Some(&tools),
            &ToolPolicy::default(),
        )
        .expect("an undeclared name is carried, and Codewhale judges it");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "shell");

        // The wire *shape* is still enforced, because a malformed call is
        // something no harness can act on.
        assert!(
            parse_assistant_message(
                r#"{"type":"tool_calls","tool_calls":[{"arguments":{}}]}"#,
                Some(&tools),
                &ToolPolicy::default(),
            )
            .is_err(),
            "a call with no function name cannot be carried"
        );
        assert!(
            parse_assistant_message(
                r#"{"type":"tool_calls","tool_calls":[{"function":{"name":"read_file","arguments":"not json"}}]}"#,
                Some(&tools),
                &ToolPolicy::default(),
            )
            .is_err(),
            "arguments must decode to a JSON object"
        );
    }

    #[test]
    fn final_json_is_unwrapped_and_plain_text_remains_a_final_answer() {
        assert_eq!(
            parse_assistant_message(
                r#"{"type":"final","content":"done"}"#,
                None,
                &ToolPolicy::default()
            )
            .expect("final")["content"],
            "done"
        );
        assert_eq!(
            parse_assistant_message("ordinary answer", None, &ToolPolicy::default())
                .expect("plain answer")["content"],
            "ordinary answer"
        );
    }

    #[test]
    fn malformed_final_envelope_is_salvaged_instead_of_shown_raw() {
        // Exactly what the model produced live: inner quotes left unescaped, so
        // the envelope is not valid JSON.
        let raw = r#"{"type":"final","content":"That does not prove it is "a Rust + Node project".\n\nWant me to look?"}"#;
        let assistant =
            parse_assistant_message(raw, None, &ToolPolicy::default()).expect("salvaged");
        let content = assistant["content"].as_str().expect("content");
        assert!(
            !content.contains("\"type\":\"final\""),
            "the envelope wrapper must not leak: {content}"
        );
        assert!(content.contains("a Rust + Node project"));
        assert!(
            content.contains('\n'),
            "escapes must be decoded: {content:?}"
        );
    }

    #[test]
    fn accepts_images_for_bridging_and_rejects_invalid_tool_arguments() {
        assert!(
            validate_request(&CompletionRequest {
                model: MODEL_ID.into(),
                messages: vec![json!({"role":"user","content":[{"type":"image_url"}]})],
                tools: None,
                tool_choice: None,
                stream: None,
            })
            .is_ok()
        );
        assert!(
            parse_assistant_message(
                r#"{"type":"tool_calls","tool_calls":[{"function":{"name":"read_file","arguments":"[]"}}]}"#,
                Some(&[json!({"function":{"name":"read_file"}})]),
                &ToolPolicy::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn tool_policy_filters_the_forwarded_catalog_and_accepts_extras() {
        let catalog = [
            json!({"type":"function","function":{"name":"read_file"}}),
            json!({"type":"function","function":{"name":"tool_search"}}),
            json!({"type":"function","function":{"name":"mcp_fetch"}}),
        ];

        // Forward-all keeps the whole catalog.
        assert_eq!(ToolPolicy::default().forwarded(&catalog).len(), 3);

        // Slim mode keeps only essentials + search tools.
        let slim = ToolPolicy {
            forward_all: false,
            essential: vec!["read_file".to_owned()],
            search: vec!["tool_search".to_owned()],
            allow_extra: vec!["mcp_fetch".to_owned()],
        };
        let forwarded = slim.forwarded(&catalog);
        let names: Vec<&str> = forwarded
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, vec!["read_file", "tool_search"]);

        // allow_extra accepts an MCP tool that the trimmed catalog does not declare.
        let assistant = parse_assistant_message(
            r#"{"type":"tool_calls","tool_calls":[{"function":{"name":"mcp_fetch","arguments":{}}}]}"#,
            Some(&catalog[..1]),
            &slim,
        )
        .expect("extra tool accepted");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "mcp_fetch");
    }

    #[test]
    fn relay_starts_once_when_fresh_and_sends_only_the_tail_on_continuation() {
        let options = BridgeOptions::default();
        let mut relay = ConversationRelay::new(true);
        let tools = vec![json!({"type":"function","function":{"name":"read_file"}})];

        let first = CompletionRequest {
            model: MODEL_ID.into(),
            messages: vec![
                json!({"role":"system","content":"SYS"}),
                json!({"role":"user","content":"USER-QUESTION-MARKER"}),
            ],
            tools: Some(tools.clone()),
            tool_choice: None,
            stream: None,
        };
        let (prompt, reset) = relay.prepare(&first, &options).expect("prepare first");
        assert!(reset, "a fresh session must open a new platform chat");
        assert!(prompt.contains("USER-QUESTION-MARKER"));
        relay
            .finish(
                &first,
                r#"{"type":"tool_calls","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":{"path":"a"}}}]}"#.into(),
                &options,
            )
            .expect("finish first");

        // Codewhale rewrites the assistant message (adds reasoning_content,
        // normalises content) and appends the tool result.
        let continuation = CompletionRequest {
            model: MODEL_ID.into(),
            messages: vec![
                json!({"role":"system","content":"SYS"}),
                json!({"role":"user","content":"USER-QUESTION-MARKER"}),
                json!({"role":"assistant","content":"","reasoning_content":"(reasoning omitted)","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":{"path":"a"}}}]}),
                json!({"role":"tool","tool_call_id":"c1","content":"TOOL-OUTPUT-MARKER"}),
            ],
            tools: Some(tools.clone()),
            tool_choice: None,
            stream: None,
        };
        let (prompt, reset) = relay
            .prepare(&continuation, &options)
            .expect("prepare continuation");
        assert!(
            !reset,
            "a tool-result continuation must not restart the platform chat"
        );
        assert!(prompt.contains("TOOL-OUTPUT-MARKER"));
        assert!(
            !prompt.contains("USER-QUESTION-MARKER"),
            "the continuation delta must not replay the whole transcript"
        );
    }

    #[test]
    fn first_turn_of_a_resumed_run_feeds_the_whole_transcript_without_a_new_chat() {
        let options = BridgeOptions::default();
        let relay = ConversationRelay::new(false); // resuming a linked chat

        // Codewhale resumes with an existing transcript.
        let resumed = CompletionRequest {
            model: MODEL_ID.into(),
            messages: vec![
                json!({"role":"system","content":"SYS"}),
                json!({"role":"user","content":"EARLIER-TURN"}),
                json!({"role":"assistant","content":"EARLIER-ANSWER"}),
                json!({"role":"user","content":"NEW-TURN"}),
            ],
            tools: None,
            tool_choice: None,
            stream: None,
        };
        let (prompt, reset) = relay.prepare(&resumed, &options).expect("prepare");
        assert!(!reset, "a resumed link must not open a new chat");
        assert!(
            prompt.contains("EARLIER-TURN"),
            "the browser must be caught up with the whole transcript"
        );
        assert!(prompt.contains("NEW-TURN"));
    }

    /// The failing case, reproduced without a model: a reply that promises to act
    /// is re-asked instead of being delivered as a final answer.
    ///
    /// This is the reported text verbatim. On this transport prose ends the turn,
    /// so before this the loop stopped with nothing done and no error.
    #[tokio::test]
    async fn a_promise_to_act_is_re_asked_instead_of_ending_the_turn() {
        install_crypto_provider();
        let ui = Arc::new(FakeUi {
            replies: StdMutex::new(vec![
                "I want to check two things before I give you a straight opinion: the repo's \
                 own naming footprint, and whether Gemini has any API-level support. Let me look."
                    .to_owned(),
                r#"{"type":"tool_calls","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":{"path":"README.md"}}}]}"#
                    .to_owned(),
            ]),
            prompts: StdMutex::new(Vec::new()),
        });
        let state = ServerState::new("secret", ui.clone(), Arc::new(FakeAudit));
        let address = serve(state).await;

        let response = post(
            address,
            "secret",
            &request(
                vec![json!({"role":"user","content":"what is this project?"})],
                false,
            ),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("completion json");
        assert_eq!(
            body["choices"][0]["finish_reason"], "tool_calls",
            "a promise to act must not end the turn: {body}"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "read_file"
        );

        let prompts = ui.prompts.lock().expect("prompts lock").clone();
        assert_eq!(prompts.len(), 2, "exactly one re-ask, not a loop");
        assert!(
            prompts[1].0.contains("neither the tool_calls JSON object"),
            "the re-ask must say what was wrong: {}",
            prompts[1].0
        );
        assert!(!prompts[1].1, "the re-ask continues the same conversation");
    }

    #[tokio::test]
    async fn a_marked_final_answer_is_delivered_without_a_re_ask() {
        install_crypto_provider();
        let ui = Arc::new(FakeUi {
            replies: StdMutex::new(vec!["Here is the answer.\n\nIt is a bridge.".to_owned()]),
            prompts: StdMutex::new(Vec::new()),
        });
        let state = ServerState::new("secret", ui.clone(), Arc::new(FakeAudit));
        let address = serve(state).await;
        let response = post(
            address,
            "secret",
            &request(
                vec![json!({"role":"user","content":"what is this?"})],
                false,
            ),
            None,
        )
        .await;
        let body: Value = response.json().await.expect("completion json");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(ui.prompts.lock().expect("prompts lock").len(), 1);
    }

    #[tokio::test]
    async fn a_retry_that_fails_again_is_delivered_rather_than_lost() {
        install_crypto_provider();
        let ui = Arc::new(FakeUi {
            replies: StdMutex::new(vec![
                "Let me look.".to_owned(),
                "Second try, still not in either form.".to_owned(),
            ]),
            prompts: StdMutex::new(Vec::new()),
        });
        let state = ServerState::new("secret", ui.clone(), Arc::new(FakeAudit));
        let address = serve(state).await;
        let response = post(
            address,
            "secret",
            &request(
                vec![json!({"role":"user","content":"what is this?"})],
                false,
            ),
            None,
        )
        .await;
        let body: Value = response.json().await.expect("completion json");
        // Bounded: one re-ask, then whatever came back is the answer.
        assert_eq!(ui.prompts.lock().expect("prompts lock").len(), 2);
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert!(
            body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Second try"),
            "the second reply is delivered, never dropped: {body}"
        );
    }

    #[test]
    fn the_re_ask_rule_only_fires_on_a_turn_that_supplied_tools() {
        let tools = vec![json!({"type":"function","function":{"name":"read_file"}})];
        // The reported failure: prose that promises action, with tools on offer.
        assert!(needs_protocol_retry("Let me look.", Some(&tools)));
        // A marked answer is a final answer, whatever else it says.
        assert!(!needs_protocol_retry(
            "Here is the answer.\n\nDone.",
            Some(&tools)
        ));
        // An envelope attempt is the parser's business, not the nudge's.
        assert!(!needs_protocol_retry("{not json at all", Some(&tools)));
        // No tools supplied: prose *is* the answer.
        assert!(!needs_protocol_retry("Let me look.", None));
        assert!(!needs_protocol_retry("Let me look.", Some(&[])));
        assert!(!needs_protocol_retry("", Some(&tools)));
    }

    #[test]
    fn a_tool_call_written_after_the_models_reasoning_is_still_a_tool_call() {
        // Verbatim shape from a live turn: the model decided to read a file, wrote
        // its reasoning, and appended the envelope. Requiring the reply to be
        // JSON-only threw the call away and printed the JSON at the user.
        let reply = concat!(
            "Here is the answer.\n\n",
            "This is doable, but I'd want to inspect the repo before promising anything. ",
            "Concretely: a provider module, a tool schema mapping, and grounding. ",
            "To give you something concrete rather than hand-wavy, I should look at ",
            "what's actually in the workspace first. Let me read a couple of files.\n",
            r#"{"type":"tool_calls","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":{"path":"README.md"}}}]}"#,
        );
        let tools = vec![json!({"type":"function","function":{"name":"read_file"}})];
        let message = parse_assistant_message(reply, Some(&tools), &ToolPolicy::default())
            .expect("the embedded call must be parsed");
        let calls = message["tool_calls"]
            .as_array()
            .expect("a tool_calls array, not prose");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        // `arguments` travels as a JSON string, the OpenAI wire shape.
        let arguments: Value = serde_json::from_str(
            calls[0]["function"]["arguments"]
                .as_str()
                .expect("arguments are a JSON string"),
        )
        .expect("arguments decode as JSON");
        assert_eq!(arguments["path"], "README.md");
        // The reasoning the model wrote is kept, not discarded.
        assert!(
            message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Let me read a couple of files"),
            "the prose before the call is part of the message: {message}"
        );
    }

    #[test]
    fn a_fenced_envelope_after_prose_is_also_found() {
        let reply = "Sure — let me check that file.\n\n```json\n\
                     {\"type\":\"tool_calls\",\"tool_calls\":[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.md\"}}}]}\n```";
        let tools = vec![json!({"type":"function","function":{"name":"read_file"}})];
        let message = parse_assistant_message(reply, Some(&tools), &ToolPolicy::default())
            .expect("the fenced call must be parsed");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn prose_that_only_mentions_tool_calls_stays_prose() {
        // A reply that talks *about* tool calls, without an envelope, must not be
        // turned into an action — otherwise a discussion becomes a stray call.
        let reply = "Here is the answer.\n\nEmpty tool_calls arrays are rejected, and a tool \
                     call must name a tool from the catalog.";
        let tools = vec![json!({"type":"function","function":{"name":"read_file"}})];
        let message = parse_assistant_message(reply, Some(&tools), &ToolPolicy::default())
            .expect("plain prose");
        assert!(
            message.get("tool_calls").is_none(),
            "prose must stay prose: {message}"
        );
        assert!(
            message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Empty")
        );
    }

    #[test]
    fn relay_forwards_codewhales_system_briefing_by_default() {
        // The model is a Codewhale coder, so it gets Codewhale's own briefing:
        // the project context, the constitution, the skills index. Bridging the
        // real thing beats describing it — an earlier design replaced this with
        // a paragraph about how the session was reached, and the model answered
        // the user by reciting that paragraph back.
        let relay = ConversationRelay::new(false);
        let request = CompletionRequest {
            model: MODEL_ID.into(),
            messages: vec![
                json!({"role":"system","content":"PROJECT-BRIEFING-MARKER"}),
                json!({"role":"user","content":"USER-MARKER"}),
            ],
            tools: None,
            tool_choice: None,
            stream: None,
        };
        let (prompt, _) = relay
            .prepare(&request, &BridgeOptions::default())
            .expect("prepare");
        assert!(prompt.contains("USER-MARKER"));
        assert!(
            prompt.contains("PROJECT-BRIEFING-MARKER"),
            "Codewhale's system briefing must be forwarded by default"
        );

        // Still droppable, for a smaller prompt: the option is a switch, not a
        // removal.
        let dropping = BridgeOptions {
            forward_system_prompt: false,
            ..BridgeOptions::default()
        };
        let relay = ConversationRelay::new(false);
        let (prompt, _) = relay.prepare(&request, &dropping).expect("prepare");
        assert!(prompt.contains("USER-MARKER"));
        assert!(!prompt.contains("PROJECT-BRIEFING-MARKER"));
    }

    struct SlowUi {
        delay: Duration,
    }

    #[async_trait]
    impl ChatUi for SlowUi {
        async fn send(&self, _prompt: &str, _start_new_chat: bool) -> Result<String, String> {
            tokio::time::sleep(self.delay).await;
            Ok(r#"{"type":"final","content":"slow answer"}"#.to_owned())
        }
    }

    #[tokio::test]
    async fn streamed_completion_flushes_headers_before_a_slow_browser_turn() {
        install_crypto_provider();
        let state = ServerState::new(
            "secret",
            Arc::new(SlowUi {
                delay: Duration::from_millis(1500),
            }),
            Arc::new(FakeAudit),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("serve");
        });

        let started = std::time::Instant::now();
        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&request(vec![json!({"role":"user","content":"hi"})], true))
            .send()
            .await
            .expect("streaming response");
        let headers_at = started.elapsed();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            headers_at < Duration::from_millis(900),
            "response headers must precede the slow browser turn (got {headers_at:?})"
        );
        let body = response.text().await.expect("stream body");
        assert!(body.contains("slow answer"), "body was {body}");
        server.abort();
    }

    #[test]
    fn partial_envelopes_stream_and_tool_calls_do_not() {
        // A JSON envelope still being written decodes to its content so far.
        assert_eq!(partial_content(r#"{"type":"final","content":"Hel"#), "Hel");
        assert_eq!(
            partial_content(r#"{"type":"final","content":"Hel\nlo"}"#),
            "Hel\nlo"
        );
        // A truncated escape is held back until the next byte lands.
        assert_eq!(
            partial_content(r#"{"type":"final","content":"line\"#),
            "line"
        );
        // Plain text streams verbatim, fences stripped.
        assert_eq!(partial_content("hello there"), "hello there");
        assert_eq!(partial_content("```json\n{\"type\":\"final\""), "");
        // A tool-call envelope is never user-visible text.
        assert_eq!(
            partial_content(r#"{"type":"tool_calls","tool_calls":[{"id":"call_1"}]}"#),
            ""
        );
        // ...but an answer that merely mentions the word still streams.
        assert_eq!(
            partial_content(r#"{"type":"final","content":"I will use tool_calls now"}"#),
            "I will use tool_calls now"
        );
    }

    #[test]
    fn content_streamer_emits_only_the_new_surplus() {
        let mut streamer = ContentStreamer::default();
        assert_eq!(
            streamer.push(r#"{"type":"final","content":"He"#),
            Some("He".to_owned())
        );
        assert_eq!(
            streamer.push(r#"{"type":"final","content":"Hello"#),
            Some("llo".to_owned())
        );
        // The same snapshot twice emits nothing.
        assert_eq!(streamer.push(r#"{"type":"final","content":"Hello"#), None);
        assert_eq!(
            streamer.push(r#"{"type":"final","content":"Hello world"}"#),
            Some(" world".to_owned())
        );
    }

    #[test]
    fn api_transport_extracts_text_by_framing() {
        assert_eq!(
            extract_api_text(config::ApiFraming::Text, "content", "raw"),
            "raw"
        );
        assert_eq!(
            extract_api_text(
                config::ApiFraming::Json,
                "choices.0.message.content",
                r#"{"choices":[{"message":{"content":"hi"}}]}"#
            ),
            "hi"
        );
        let sse = "data: {\"content\":\"Hel\"}\n\ndata: {\"content\":\"lo\"}\n\ndata: [DONE]\n\n";
        assert_eq!(
            extract_api_text(config::ApiFraming::Sse, "content", sse),
            "Hello"
        );
        // A malformed frame is skipped rather than failing the whole turn.
        assert_eq!(
            extract_api_text(config::ApiFraming::Sse, "content", "data: not-json\n\n"),
            ""
        );
    }

    /// A UI that reports the visible reply growing, then finishes it.
    struct StreamingUi;

    #[async_trait]
    impl ChatUi for StreamingUi {
        async fn send(&self, _prompt: &str, _start_new_chat: bool) -> Result<String, String> {
            unreachable!("the streaming path is always used")
        }

        async fn send_streaming(
            &self,
            _prompt: &str,
            _start_new_chat: bool,
            snapshots: tokio::sync::mpsc::Sender<String>,
        ) -> Result<String, String> {
            for partial in ["Hel", "Hello", "Hello world"] {
                let _ = snapshots.send(partial.to_owned()).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(r#"{"type":"final","content":"Hello world"}"#.to_owned())
        }

        async fn model_label(&self) -> Option<String> {
            Some("DeepSeek-V4".to_owned())
        }
    }

    #[tokio::test]
    async fn streamed_turn_emits_content_deltas_as_the_reply_grows() {
        install_crypto_provider();
        let state = ServerState::new("secret", Arc::new(StreamingUi), Arc::new(FakeAudit));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("serve");
        });

        let response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&request(vec![json!({"role":"user","content":"hi"})], true))
            .send()
            .await
            .expect("streaming response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.expect("stream body");
        // Incremental chunks, in order, well before the turn finished.
        let hel = body.find(r#""content":"Hel""#).expect("first delta");
        let lo = body.find(r#""content":"lo""#).expect("second delta");
        let world = body.find(r#""content":" world""#).expect("third delta");
        assert!(
            hel < lo && lo < world,
            "deltas must arrive in order: {body}"
        );
        assert!(
            body.contains(r#""finish_reason":"stop""#),
            "body was {body}"
        );
        assert!(body.trim_end().ends_with("data: [DONE]"), "body was {body}");
        // The final content is not duplicated after the deltas.
        assert_eq!(
            body.matches(r#""content":"Hello world""#).count(),
            0,
            "body was {body}"
        );
        server.abort();
    }

    struct FakeUi {
        replies: StdMutex<Vec<String>>,
        prompts: StdMutex<Vec<(String, bool)>>,
    }

    #[async_trait]
    impl ChatUi for FakeUi {
        async fn send(&self, prompt: &str, start_new_chat: bool) -> Result<String, String> {
            self.prompts
                .lock()
                .expect("prompts lock")
                .push((prompt.to_owned(), start_new_chat));
            let mut replies = self.replies.lock().expect("replies lock");
            if replies.is_empty() {
                return Err("no mock response".into());
            }
            Ok(replies.remove(0))
        }
    }

    struct FakeAudit;
    #[async_trait]
    impl AuditSink for FakeAudit {
        async fn append(&self, _record: Value) -> Result<(), String> {
            Ok(())
        }
    }

    /// A UI that never answers, and knows why it did not.
    struct FailingUi;

    #[async_trait]
    impl ChatUi for FailingUi {
        async fn send(&self, _prompt: &str, _start_new_chat: bool) -> Result<String, String> {
            Err("no visible assistant reply within 300s (composer empty)".to_owned())
        }

        async fn diagnose(&self, _error: &str) -> Option<Failure> {
            Some(Failure {
                kind: "dns".to_owned(),
                blame: "network".to_owned(),
                detail: "net::ERR_NAME_NOT_RESOLVED".to_owned(),
                http_status: None,
            })
        }
    }

    #[derive(Default)]
    struct RecordingTurns {
        turns: StdMutex<Vec<TurnRecord>>,
    }

    #[async_trait]
    impl TurnSink for RecordingTurns {
        async fn record(&self, turn: TurnRecord) -> Result<(), String> {
            self.turns.lock().expect("turns lock").push(turn);
            Ok(())
        }
    }

    async fn serve(state: ServerState) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("serve");
        });
        address
    }

    #[tokio::test]
    async fn a_failed_turn_is_recorded_with_the_transports_diagnosis() {
        install_crypto_provider();
        let turns = Arc::new(RecordingTurns::default());
        let state = ServerState::new("secret", Arc::new(FailingUi), Arc::new(FakeAudit))
            .with_turns(turns.clone());
        let address = serve(state).await;

        let response = post(
            address,
            "secret",
            &request(vec![json!({"role":"user","content":"hi"})], false),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let recorded = turns.turns.lock().expect("turns lock").clone();
        assert_eq!(recorded.len(), 1, "a failed turn must still be a record");
        assert_eq!(recorded[0].finish_reason, "error");
        let failure = recorded[0]
            .failure
            .clone()
            .expect("the failure must be recorded");
        assert_eq!(failure.kind, "dns");
        assert_eq!(failure.blame, "network");
        assert!(
            !failure.is_service_fault(),
            "a DNS failure must not be blamed on the service"
        );
    }

    #[tokio::test]
    async fn a_refused_request_is_recorded_as_the_wrappers_fault() {
        install_crypto_provider();
        let turns = Arc::new(RecordingTurns::default());
        let state = ServerState::new(
            "secret",
            Arc::new(FakeUi {
                replies: StdMutex::new(Vec::new()),
                prompts: StdMutex::new(Vec::new()),
            }),
            Arc::new(FakeAudit),
        )
        .with_turns(turns.clone());
        let address = serve(state).await;

        // No messages at all: refused before the browser is reached.
        let response = post(address, "secret", &request(Vec::new(), false), None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let recorded = turns.turns.lock().expect("turns lock").clone();
        assert_eq!(recorded.len(), 1, "a refused request is still a turn");
        let failure = recorded[0].failure.clone().expect("recorded");
        assert_eq!(failure.kind, "request");
        assert_eq!(failure.blame, "wrapper");
        assert!(!failure.is_service_fault());
    }

    fn request(messages: Vec<Value>, stream: bool) -> CompletionRequest {
        CompletionRequest {
            model: MODEL_ID.into(),
            messages,
            tools: Some(vec![json!({
                "type":"function",
                "function":{"name":"read_file","parameters":{"type":"object"}}
            })]),
            tool_choice: None,
            stream: Some(stream),
        }
    }

    async fn post(
        address: std::net::SocketAddr,
        token: &str,
        request: &CompletionRequest,
        origin: Option<&str>,
    ) -> reqwest::Response {
        let client = reqwest::Client::new();
        let mut builder = client
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(token)
            .json(request);
        if let Some(origin) = origin {
            builder = builder.header("Origin", origin);
        }
        builder.send().await.expect("mock relay response")
    }

    #[tokio::test]
    async fn coding_turn_relays_tool_result_then_final_answer_and_denies_cors() {
        install_crypto_provider();
        let ui = Arc::new(FakeUi {
            replies: StdMutex::new(vec![
                r#"{"type":"tool_calls","tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":{"path":"src/main.rs"}}}]}"#.into(),
                r#"{"type":"final","content":"The function returns 42."}"#.into(),
            ]),
            prompts: StdMutex::new(Vec::new()),
        });
        let state = ServerState::new("local-test-token", ui.clone(), Arc::new(FakeAudit));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("serve");
        });

        let initial = request(
            vec![
                json!({"role":"system","content":"Use read_file before answering."}),
                json!({"role":"user","content":"What does main.rs return?"}),
            ],
            true,
        );
        let response = post(
            address,
            "local-test-token",
            &initial,
            Some("https://attacker.invalid"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        let event_stream = response.text().await.expect("stream response");
        assert!(event_stream.contains("read_file"));
        assert!(event_stream.contains("tool_calls"));

        let continuation = request(
            vec![
                json!({"role":"system","content":"Use read_file before answering."}),
                json!({"role":"user","content":"What does main.rs return?"}),
                json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":"call-1","type":"function","function":{"name":"read_file","arguments":r#"{"path":"src/main.rs"}"#}
                }]}),
                json!({"role":"tool","tool_call_id":"call-1","content":"fn main() { 42 }"}),
            ],
            false,
        );
        let response = post(address, "local-test-token", &continuation, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("final response JSON");
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "The function returns 42."
        );
        let prompts = ui.prompts.lock().expect("prompts lock");
        assert_eq!(prompts.len(), 2);
        assert!(!prompts[0].1);
        assert!(!prompts[1].1);
        assert!(prompts[1].0.contains("fn main() { 42 }"));
        assert!(!prompts[1].0.contains("What does main.rs return?"));
        server.abort();
    }

    #[tokio::test]
    async fn both_models_are_advertised_with_a_price_that_is_never_unknown() {
        install_crypto_provider();
        let state = ServerState::new(
            "secret",
            Arc::new(FakeUi {
                replies: StdMutex::new(Vec::new()),
                prompts: StdMutex::new(Vec::new()),
            }),
            Arc::new(FakeAudit),
        );
        let address = serve(state).await;
        let body: Value = reqwest::Client::new()
            .get(format!("http://{address}/v1/models"))
            .bearer_auth("secret")
            .send()
            .await
            .expect("models response")
            .json()
            .await
            .expect("models json");

        let ids: Vec<&str> = body["data"]
            .as_array()
            .expect("a model list")
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        assert!(
            ids.contains(&MODEL_ID),
            "the chat model must be advertised: {ids:?}"
        );
        assert!(
            ids.contains(&PRO_MODEL_ID),
            "the pro model must be advertised: {ids:?}"
        );

        // Whatever asks, the answer is a non-empty string -- never null, never
        // absent, so a caller has nothing to render as "unknown".
        for entry in body["data"].as_array().expect("a model list") {
            let pricing = entry["pricing"]
                .as_str()
                .unwrap_or_else(|| panic!("every model needs a pricing string: {entry}"));
            assert_eq!(pricing, PRICING_LABEL);
            assert!(!pricing.trim().is_empty());
            assert!(entry["input_per_million"].is_number());
            assert!(entry["output_per_million"].is_number());
        }
    }

    #[test]
    fn only_the_models_this_wrapper_serves_are_accepted() {
        assert!(serves_model(MODEL_ID));
        assert!(serves_model(PRO_MODEL_ID));
        assert!(!serves_model("gpt-5.5"));
        assert!(!serves_model(""));
    }

    #[tokio::test]
    async fn the_pro_model_is_served_and_a_stranger_is_still_refused() {
        install_crypto_provider();
        let state = ServerState::new(
            "secret",
            Arc::new(FakeUi {
                replies: StdMutex::new(vec!["ok".to_owned()]),
                prompts: StdMutex::new(Vec::new()),
            }),
            Arc::new(FakeAudit),
        );
        let address = serve(state).await;

        let mut pro = request(vec![json!({"role":"user","content":"hi"})], false);
        pro.model = PRO_MODEL_ID.to_owned();
        let response = post(address, "secret", &pro, None).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the pro model must be served, not refused"
        );
        // Answered by the same page, but it must not be *reported* as the plain
        // model: the model id is the only thing that tells the caller which one
        // it asked for.
        let body: Value = response.json().await.expect("completion json");
        assert_eq!(body["model"], PRO_MODEL_ID);

        let mut stranger = request(vec![json!({"role":"user","content":"hi"})], false);
        stranger.model = "gpt-5.5".to_owned();
        let response = post(address, "secret", &stranger, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn loopback_api_requires_token_and_never_advertises_cors() {
        install_crypto_provider();
        let state = ServerState::new(
            "secret",
            Arc::new(FakeUi {
                replies: StdMutex::new(Vec::new()),
                prompts: StdMutex::new(Vec::new()),
            }),
            Arc::new(FakeAudit),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("serve");
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/v1/models"))
            .header("Origin", "https://attacker.invalid")
            .send()
            .await
            .expect("unauthorized response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );

        let response = reqwest::Client::new()
            .get(format!("http://{address}/v1/models"))
            .bearer_auth("secret")
            .header("Origin", "https://attacker.invalid")
            .send()
            .await
            .expect("authenticated cross-origin response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );

        let response = reqwest::Client::new()
            .request(
                reqwest::Method::OPTIONS,
                format!("http://{address}/v1/chat/completions"),
            )
            .header("Origin", "https://attacker.invalid")
            .header("Access-Control-Request-Method", "POST")
            .header(
                "Access-Control-Request-Headers",
                "authorization,content-type",
            )
            .send()
            .await
            .expect("cross-origin preflight response");
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        server.abort();
    }
}
