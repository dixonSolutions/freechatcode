//! Configuration for the wrapper.
//!
//! Two layers, lowest precedence first:
//!
//! 1. **Committed defaults** — `assets/config.default.toml`, embedded into the
//!    binary with [`include_str!`]. Shipped with the package, never read from
//!    disk at runtime.
//! 2. **User config** — `~/.codewhale/freechatcode/config.toml` (honoring
//!    `$CODEWHALE_HOME`). Per-machine values and overrides of any default.
//!
//! Command-line flags and environment variables are applied on top of the
//! merged result by `main.rs` (CLI > env > user config > default).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// Shared defaults committed to the repository and embedded in the binary.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../assets/config.default.toml");

/// The user-config location under the Codewhale home directory.
#[must_use]
pub fn user_config_path(home: &Path) -> PathBuf {
    home.join("freechatcode").join("config.toml")
}

/// One-time migration from the pre-rename project directory
/// (`<home>/deepchatcode`) to the current one (`<home>/freechatcode`).
///
/// Non-destructive: the legacy directory is renamed in place, and is left
/// untouched when the current directory already exists.
pub fn migrate_legacy_home(home: &Path) {
    let legacy = home.join("deepchatcode");
    let current = home.join("freechatcode");
    if legacy.exists() && !current.exists() {
        if let Err(error) = std::fs::rename(&legacy, &current) {
            eprintln!(
                "freechatcode: could not migrate legacy config dir {} to {}: {error}",
                legacy.display(),
                current.display()
            );
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub chat: ChatConfig,
    pub selectors: Selectors,
    pub timeouts: Timeouts,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub relay: RelayConfig,
    #[serde(default)]
    pub codewhale: CodewhaleConfig,
    #[serde(default)]
    pub browser: BrowserConfig,
    #[serde(default)]
    pub transport: TransportConfig,
    /// `show` (default) or `silent`. A preset over `[browser] headless` and
    /// `[transport] mode`, not a lock on them — see [`RunMode`].
    #[serde(default)]
    pub mode: RunMode,
}

/// Controls which of Codewhale's tool definitions are forwarded to the chat.
/// Names live here, never in code.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolsConfig {
    /// Forward Codewhale's full tool catalog. When false, only `essential` +
    /// `search` are sent, leaning on the search tool for discovery.
    #[serde(default = "default_true")]
    pub forward_all: bool,
    /// Tool names always forwarded, even when `forward_all` is false.
    #[serde(default)]
    pub essential: Vec<String>,
    /// Discovery/search tool names, always forwarded.
    #[serde(default = "default_search_tools")]
    pub search: Vec<String>,
    /// Extra tool-call names to accept even if the request does not declare
    /// them (e.g. MCP tools surfaced later in the conversation).
    #[serde(default)]
    pub allow_extra: Vec<String>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            forward_all: true,
            essential: Vec::new(),
            search: default_search_tools(),
            allow_extra: Vec::new(),
        }
    }
}

fn default_search_tools() -> Vec<String> {
    vec!["tool_search".to_owned()]
}

/// Relay behaviour that is not about the browser target itself.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayConfig {
    /// Show a desktop notification when the wrapper has to realign the browser
    /// (for example, a linked conversation is no longer reachable).
    #[serde(default = "default_true")]
    pub desktop_notifications: bool,
    /// Forward Codewhale's own system message into the chat. It carries the full
    /// project briefing, so it stays off by default and the instruction text
    /// identifies the session instead.
    #[serde(default)]
    pub forward_system_prompt: bool,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            desktop_notifications: true,
            forward_system_prompt: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatConfig {
    pub url: String,
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub routed_url_pattern: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Selectors {
    pub composer: String,
    pub assistant: String,
    pub send: String,
    pub new_chat: String,
    pub search_toggle: String,
    pub thinking_toggle: String,
    pub file_upload: String,
    /// Element whose text names the model in use (e.g. a model picker button).
    /// Empty disables model attribution from the page; the turn log then records
    /// the model as "unknown".
    #[serde(default)]
    pub model_label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Timeouts {
    pub login_wait_secs: u64,
    /// How long to wait for the composer in headless mode before deciding that
    /// sign-in is required and reopening a visible browser.
    #[serde(default = "default_login_probe_secs")]
    pub login_probe_secs: u64,
    pub response_secs: u64,
    /// How often to re-read the visible page while waiting for it to settle.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// How many consecutive identical polls of the newest assistant message
    /// count as "the reply stopped growing".
    ///
    /// This is a quiet window, and it was far too short: at 150 ms per poll,
    /// three polls meant half a second of stillness, and the page renders in
    /// bursts with longer gaps than that. The result was a reply declared
    /// complete mid-sentence — measured once against the page itself, which
    /// held **992 characters** of an answer the relay had recorded as 208 — and
    /// from the user's seat that looks like a turn stuck halfway through
    /// printing. Twenty polls is about three seconds of stillness, which a
    /// mid-reply pause does not reach. The cost is a few seconds between the
    /// last streamed word and the end of the turn, and that is the right trade:
    /// the words are already on screen as they arrive.
    #[serde(default = "default_settle_polls")]
    pub settle_polls: u32,
    /// Upper bound on a single Playwright action, so one awkward element cannot
    /// consume a whole turn.
    #[serde(default = "default_action_secs")]
    pub action_secs: u64,
    /// Upper bound on a single page navigation.
    #[serde(default = "default_navigation_secs")]
    pub navigation_secs: u64,
    /// How long to wait for a linked conversation to render before treating the
    /// link as stale.
    #[serde(default = "default_link_probe_secs")]
    pub link_probe_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CodewhaleConfig {
    /// Explicit path to the `codewhale` executable.
    #[serde(default)]
    pub binary: Option<String>,
    /// Path to the instruction text handed to the chat model. When unset, the
    /// committed `assets/system-prompt.md` embedded in the binary is used.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// How the relay obtains a browser to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserMode {
    /// Launch a dedicated persistent Chromium profile owned by the wrapper.
    #[default]
    Managed,
    /// Attach to an already-running Chromium over CDP — your own signed-in
    /// browser session (start it with `--remote-debugging-port=9222`).
    Attach,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserConfig {
    /// `managed` (default) or `attach`. Parsed from `[browser] mode`.
    #[serde(default)]
    pub mode: BrowserMode,
    /// Browser profile directory (used when `mode = "managed"`).
    #[serde(default)]
    pub profile_dir: Option<String>,
    /// CDP endpoint used when `mode = "attach"`, e.g. `http://127.0.0.1:9222`.
    #[serde(default)]
    pub cdp_endpoint: Option<String>,
    /// Run Chromium headless unless sign-in is required. Defaults to true.
    #[serde(default = "default_true")]
    pub headless: bool,
    /// Keep the browser running between turns. When false, the browser is
    /// closed after every reply and relaunched for the next turn: it frees
    /// memory at the cost of a relaunch (and, in `attach` mode, a re-attach).
    #[serde(default = "default_true")]
    pub keep_alive: bool,
    /// Record the managed browser's page to a video in this directory. Written
    /// when the browser closes, one file per run. Useful for demos and for
    /// seeing what the page actually did after a failed turn.
    #[serde(default)]
    pub record_video_dir: Option<String>,
    /// Recording size, `"WxH"`. Unset records the page viewport as-is.
    #[serde(default)]
    pub record_video_size: Option<String>,
    /// How often to check that the browser is still there while the bridge sits
    /// idle. When it is gone it is reopened on the conversation in progress, so
    /// a browser that is closed or crashes comes back in seconds rather than at
    /// the next prompt. `0` disables the check. Only applies when `keep_alive`.
    #[serde(default = "default_liveness_check_secs")]
    pub liveness_check_secs: u64,
}

/// How the wrapper presents itself while it works: a window you can watch, or
/// nothing you have to look at.
///
/// A mode is a *preset* over two independent knobs — `[browser] headless` and
/// `[transport] mode` — applied to the defaults layer, so a user who explicitly
/// set either knob in their own config keeps it (the documented resolution is
/// still CLI > env > user config > default). Being a preset rather than a lock
/// is what lets `silent` keep driving the page when the direct API path is
/// unavailable, and what lets a visible window open for sign-in in any mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum RunMode {
    /// Visible browser, prompts driven through the page.
    #[default]
    Show,
    /// Headless browser, prompts sent to the API directly.
    Silent,
}

impl RunMode {
    /// Parse the mode from a config value or a CLI argument.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "show" => Some(Self::Show),
            "silent" => Some(Self::Silent),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::Silent => "silent",
        }
    }

    /// The two knobs this mode sets: `(headless, transport)`.
    #[must_use]
    pub fn preset(self) -> (bool, TransportMode) {
        match self {
            // Watch it work: a real window, and the prompts go through it.
            Self::Show => (false, TransportMode::Gui),
            // Nothing on screen, and ask the API directly.
            Self::Silent => (true, TransportMode::Api),
        }
    }
}

/// How a completion is delivered to DeepSeek.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    /// Drive the chat page: type the prompt and read the visible reply.
    #[default]
    Gui,
    /// POST to the site's own completion endpoint from inside the page context,
    /// so the signed-in session cookies apply. This is a private endpoint, not
    /// an API contract, and it can change without notice.
    Api,
}

impl TransportMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Api => "api",
        }
    }
}

/// Response framing for the `api` transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiFraming {
    /// A stream of `data: {json}` lines (server-sent events).
    #[default]
    Sse,
    /// One JSON document.
    Json,
    /// A plain-text body.
    Text,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransportConfig {
    /// `gui` (default) or `api`.
    #[serde(default)]
    pub mode: TransportMode,
    /// Settings for `mode = "api"`.
    #[serde(default)]
    pub api: ApiTransportConfig,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            mode: TransportMode::Gui,
            api: ApiTransportConfig::default(),
        }
    }
}

/// A generic same-origin HTTP bridge. Every piece of the private request and
/// response shape is configuration, so a change to the endpoint is a config
/// edit rather than a rebuild.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiTransportConfig {
    /// Absolute URL or same-origin path POSTed from inside the chat page.
    #[serde(default = "default_api_url")]
    pub url: String,
    /// Request body template. `{messages}` is replaced with the JSON messages
    /// array, `{tools}` with the JSON tools array (or `null`), `{payload}` with
    /// the whole `{"messages":…,"tools":…}` object, and `{prompt}` with the
    /// instruction text.
    #[serde(default = "default_api_body")]
    pub body: String,
    /// How the response is framed.
    #[serde(default)]
    pub framing: ApiFraming,
    /// Dot path to the assistant text inside each response object, e.g.
    /// `choices.0.message.content` or `content`. Empty uses the whole body.
    #[serde(default = "default_api_text_path")]
    pub text_path: String,
    /// How many seconds to wait for the API call to return.
    #[serde(default = "default_api_timeout_secs")]
    pub timeout_secs: u64,
    /// Extra request headers, sent on top of the JSON content type. Some
    /// endpoints need a static authorization or client-version header. A header
    /// the server derives per request (a proof-of-work, say) cannot be supplied
    /// here — see the note in `assets/config.default.toml`.
    #[serde(default)]
    pub extra_headers: std::collections::BTreeMap<String, String>,
}

impl Default for ApiTransportConfig {
    fn default() -> Self {
        Self {
            url: default_api_url(),
            body: default_api_body(),
            framing: ApiFraming::default(),
            text_path: default_api_text_path(),
            timeout_secs: default_api_timeout_secs(),
            extra_headers: std::collections::BTreeMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_login_probe_secs() -> u64 {
    20
}

fn default_poll_ms() -> u64 {
    150
}

fn default_settle_polls() -> u32 {
    20
}

fn default_action_secs() -> u64 {
    20
}

fn default_navigation_secs() -> u64 {
    20
}

fn default_link_probe_secs() -> u64 {
    10
}

fn default_liveness_check_secs() -> u64 {
    15
}

fn default_api_url() -> String {
    "/api/v0/chat/completion".to_owned()
}

fn default_api_body() -> String {
    "{\"messages\":{messages},\"stream\":true}".to_owned()
}

fn default_api_text_path() -> String {
    "content".to_owned()
}

fn default_api_timeout_secs() -> u64 {
    300
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            mode: BrowserMode::default(),
            profile_dir: None,
            cdp_endpoint: None,
            headless: true,
            keep_alive: true,
            record_video_dir: None,
            record_video_size: None,
            liveness_check_secs: default_liveness_check_secs(),
        }
    }
}

impl BrowserConfig {
    /// The configured recording size as `(width, height)`, when valid.
    #[must_use]
    pub fn video_size(&self) -> Option<(u32, u32)> {
        let (width, height) = self.record_video_size.as_deref()?.split_once('x')?;
        Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
    }
}

impl ApiTransportConfig {
    /// Render the configured request template for one turn. `messages` and
    /// `tools` are already-serialized JSON values; `{prompt}` carries the
    /// instruction text; `{thinking}` carries the page's reasoning state.
    ///
    /// Which model answers is page state upstream, so an endpoint that can be
    /// told about it gets the flag here. A template that never names
    /// `{thinking}` cannot be told — `call_api` refuses that combination rather
    /// than answering the plain model while the caller believes it asked for pro.
    #[must_use]
    pub fn render_body(
        &self,
        messages: &Value,
        tools: Option<&Value>,
        prompt: &str,
        thinking: bool,
    ) -> String {
        let payload = serde_json::json!({"messages": messages, "tools": tools});
        let tools = tools.map_or_else(|| "null".to_owned(), Value::to_string);
        self.body
            .replace("{payload}", &payload.to_string())
            .replace("{messages}", &messages.to_string())
            .replace("{tools}", &tools)
            .replace("{prompt}", prompt)
            .replace("{thinking}", if thinking { "true" } else { "false" })
    }
}

/// Resolve a dot path such as `choices.0.message.content` against a JSON value.
/// Numeric segments index arrays.
#[must_use]
pub fn json_at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

/// The assistant text at `path` in `value`. An empty path means the value is
/// itself the text. Returns `None` when the path is missing or not a string.
#[must_use]
pub fn text_at_path(value: &Value, path: &str) -> Option<String> {
    if path.is_empty() {
        return value.as_str().map(str::to_owned);
    }
    json_at_path(value, path)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

impl Timeouts {
    #[must_use]
    pub fn login_wait(&self) -> Duration {
        Duration::from_secs(self.login_wait_secs)
    }

    #[must_use]
    pub fn login_probe(&self) -> Duration {
        Duration::from_secs(self.login_probe_secs)
    }

    #[must_use]
    pub fn response(&self) -> Duration {
        Duration::from_secs(self.response_secs)
    }

    #[must_use]
    pub fn poll(&self) -> Duration {
        Duration::from_millis(self.poll_ms.max(1))
    }

    #[must_use]
    pub fn action(&self) -> Duration {
        Duration::from_secs(self.action_secs.max(1))
    }

    #[must_use]
    pub fn navigation(&self) -> Duration {
        Duration::from_secs(self.navigation_secs.max(1))
    }

    #[must_use]
    pub fn link_probe(&self) -> Duration {
        Duration::from_secs(self.link_probe_secs.max(1))
    }
}

impl ChatConfig {
    /// Whether `raw` is an accepted HTTPS chat URL on a configured host.
    #[must_use]
    pub fn accepts_url(&self, raw: &str) -> bool {
        url::Url::parse(raw).ok().is_some_and(|url| {
            url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| self.allowed_hosts.iter().any(|allowed| allowed == host))
        })
    }

    /// Whether a captured conversation `url` is distinct enough from the base
    /// URL to be worth storing as a resumable link.
    #[must_use]
    pub fn is_resumable_url(&self, url: &str) -> bool {
        if !self.accepts_url(url) || url == self.url {
            return false;
        }
        self.routed_url_pattern.is_empty() || url.contains(&self.routed_url_pattern)
    }
}

impl Config {
    /// The embedded defaults, with no user overrides.
    #[must_use]
    pub fn defaults() -> Self {
        toml::from_str(DEFAULT_CONFIG_TOML).expect("embedded default config is valid TOML")
    }

    /// Load defaults, deep-merged with the user config at `user_path` when it
    /// exists. A missing user config is not an error.
    pub fn load(user_path: Option<&Path>) -> Result<Self, String> {
        Self::load_with_mode(user_path, None)
    }

    /// Load with an explicit run mode, e.g. from `--mode`.
    ///
    /// The mode has to be known *before* the merge, because it is applied to
    /// the defaults layer: that is what makes it a preset the user config can
    /// override rather than a value that overrides the user config. So the
    /// effective mode is resolved first — CLI flag, then the user config's own
    /// `mode`, then the default — and only then is the document merged.
    pub fn load_with_mode(
        user_path: Option<&Path>,
        cli_mode: Option<RunMode>,
    ) -> Result<Self, String> {
        let mut merged: toml::Value = toml::from_str(DEFAULT_CONFIG_TOML)
            .map_err(|error| format!("embedded default config is invalid: {error}"))?;
        let user = match user_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => Some(
                    toml::from_str::<toml::Value>(&text)
                        .map_err(|error| format!("user config {}: {error}", path.display()))?,
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "could not read user config {}: {error}",
                        path.display()
                    ));
                }
            },
            None => None,
        };

        let mode = cli_mode
            .or_else(|| {
                user.as_ref()
                    .and_then(|user| user.get("mode"))
                    .and_then(toml::Value::as_str)
                    .and_then(RunMode::parse)
            })
            .unwrap_or_default();

        let (headless, transport) = mode.preset();
        set_path(
            &mut merged,
            &["browser", "headless"],
            toml::Value::Boolean(headless),
        );
        set_path(
            &mut merged,
            &["transport", "mode"],
            toml::Value::String(transport.as_str().to_owned()),
        );

        if let Some(user) = user {
            merge_value(&mut merged, user);
        }
        // The resolved mode is part of the resolved config, so everything
        // downstream reads one value instead of re-deriving it.
        set_path(
            &mut merged,
            &["mode"],
            toml::Value::String(mode.as_str().to_owned()),
        );

        merged
            .try_into()
            .map_err(|error| format!("invalid configuration: {error}"))
    }
}

/// Set a dotted path in a TOML document, creating intermediate tables.
fn set_path(document: &mut toml::Value, path: &[&str], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = document;
    for key in parents {
        if !current.is_table() {
            *current = toml::Value::Table(toml::map::Map::new());
        }
        current = current
            .as_table_mut()
            .expect("just made a table")
            .entry((*key).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    if !current.is_table() {
        *current = toml::Value::Table(toml::map::Map::new());
    }
    current
        .as_table_mut()
        .expect("just made a table")
        .insert((*last).to_owned(), value);
}

/// Persist the resolved codewhale binary path into the user config so later
/// runs resolve it without discovery. Other keys are preserved, and an
/// unchanged value is not rewritten.
pub fn remember_binary(user_path: &Path, binary: &Path) -> Result<(), String> {
    let value = binary
        .to_str()
        .ok_or("the codewhale binary path is not valid UTF-8")?;
    let mut document = match std::fs::read_to_string(user_path) {
        Ok(text) => toml::from_str::<toml::Value>(&text)
            .map_err(|error| format!("user config {}: {error}", user_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(error) => return Err(format!("read {}: {error}", user_path.display())),
    };
    let table = document
        .as_table_mut()
        .ok_or("the user config must be a TOML table")?;
    let codewhale = table
        .entry("codewhale".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let codewhale = codewhale
        .as_table_mut()
        .ok_or("`codewhale` in the user config must be a table")?;
    if codewhale.get("binary").and_then(toml::Value::as_str) == Some(value) {
        return Ok(());
    }
    codewhale.insert("binary".to_owned(), toml::Value::String(value.to_owned()));

    if let Some(parent) = user_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
        restrict_directory(parent)?;
    }
    let text = toml::to_string_pretty(&document)
        .map_err(|error| format!("serialize user config: {error}"))?;
    std::fs::write(user_path, text)
        .map_err(|error| format!("write {}: {error}", user_path.display()))?;
    restrict_file(user_path)
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("restrict {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("restrict {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Recursively overlay `over` onto `base`: tables merge key-by-key, any other
/// value replaces.
fn merge_value(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot) => merge_value(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_defaults_parse() {
        let config = Config::defaults();
        assert_eq!(config.chat.url, "https://chat.deepseek.com");
        assert!(
            config
                .chat
                .allowed_hosts
                .contains(&"chat.deepseek.com".to_owned())
        );
        assert!(config.selectors.composer.contains("textarea"));
        assert_eq!(config.timeouts.login_wait_secs, 900);
        assert!(config.codewhale.binary.is_none());
        assert!(config.codewhale.system_prompt.is_none());
        assert!(config.tools.forward_all);
        assert_eq!(config.tools.search, vec!["tool_search".to_owned()]);
        assert!(config.tools.essential.is_empty());
    }

    #[test]
    fn embedded_defaults_describe_browser_and_transport() {
        let config = Config::defaults();
        // Browser defaults: a managed, headless, long-lived browser.
        assert_eq!(config.browser.mode, BrowserMode::Managed);
        assert!(config.browser.headless);
        assert!(config.browser.keep_alive);
        assert_eq!(config.browser.liveness_check_secs, 15);
        // Transport defaults: drive the chat page.
        assert_eq!(config.transport.mode, TransportMode::Gui);
        assert_eq!(config.transport.api.url, "/api/v0/chat/completion");
        assert_eq!(config.transport.api.framing, ApiFraming::Sse);
        assert_eq!(config.transport.api.text_path, "content");
    }

    #[test]
    fn browser_and_transport_keys_are_config_driven() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            [browser]
            mode = "attach"
            headless = false
            keep_alive = false
            cdp_endpoint = "http://127.0.0.1:9222"

            [transport]
            mode = "api"
            [transport.api]
            url = "https://chat.deepseek.com/api/v0/chat/completion"
            framing = "json"
            text_path = "choices.0.message.content"
            "#,
        )
        .expect("write user config");

        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.browser.mode, BrowserMode::Attach);
        assert!(!config.browser.headless);
        assert!(!config.browser.keep_alive);
        assert_eq!(
            config.browser.cdp_endpoint.as_deref(),
            Some("http://127.0.0.1:9222")
        );
        assert_eq!(config.transport.mode, TransportMode::Api);
        assert_eq!(config.transport.api.framing, ApiFraming::Json);
        assert_eq!(config.transport.api.text_path, "choices.0.message.content");
    }

    #[test]
    fn each_mode_presets_the_two_knobs_it_owns() {
        let show = Config::load_with_mode(None, Some(RunMode::Show)).expect("load");
        assert!(!show.browser.headless, "show means a window you can watch");
        assert_eq!(show.transport.mode, TransportMode::Gui);
        assert_eq!(show.mode, RunMode::Show);

        let silent = Config::load_with_mode(None, Some(RunMode::Silent)).expect("load");
        assert!(silent.browser.headless, "silent means no window");
        assert_eq!(silent.transport.mode, TransportMode::Api);
        assert_eq!(silent.mode, RunMode::Silent);
    }

    #[test]
    fn show_is_the_default_mode_and_parse_rejects_nonsense() {
        assert_eq!(Config::defaults().mode, RunMode::Show);
        assert_eq!(RunMode::parse("show"), Some(RunMode::Show));
        assert_eq!(RunMode::parse(" SILENT "), Some(RunMode::Silent));
        assert_eq!(RunMode::parse("loud"), None);
    }

    /// The point of calling a mode a preset: it moves the defaults, so a knob
    /// the user set themselves still wins, and the knob they did *not* set still
    /// follows the mode.
    #[test]
    fn a_mode_moves_the_defaults_and_an_explicit_user_value_still_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        // `show`, but the user does not want a window: the prompt still goes
        // through the page, which is the knob they left alone.
        std::fs::write(&path, "[browser]\nheadless = true\n").expect("write user config");
        let config = Config::load_with_mode(Some(&path), Some(RunMode::Show)).expect("load");
        assert!(
            config.browser.headless,
            "an explicit headless = true is not overridden by show"
        );
        assert_eq!(config.transport.mode, TransportMode::Gui);

        // `silent`, but the user insists on a window: it survives, and the
        // transport they did not mention still follows the mode.
        std::fs::write(&path, "[browser]\nheadless = false\n").expect("write user config");
        let config = Config::load_with_mode(Some(&path), Some(RunMode::Silent)).expect("load");
        assert!(
            !config.browser.headless,
            "an explicit headless = false survives silent"
        );
        assert_eq!(config.transport.mode, TransportMode::Api);
    }

    #[test]
    fn a_mode_in_the_user_config_is_honoured_and_the_cli_flag_beats_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "mode = \"silent\"\n").expect("write user config");

        let from_config = Config::load(Some(&path)).expect("load");
        assert_eq!(from_config.mode, RunMode::Silent);
        assert!(from_config.browser.headless);
        assert_eq!(from_config.transport.mode, TransportMode::Api);

        let from_flag = Config::load_with_mode(Some(&path), Some(RunMode::Show)).expect("load");
        assert_eq!(from_flag.mode, RunMode::Show);
        assert!(!from_flag.browser.headless);
        assert_eq!(from_flag.transport.mode, TransportMode::Gui);
    }

    #[test]
    fn api_body_template_fills_every_placeholder() {
        let template = ApiTransportConfig {
            body: "{\"m\":{messages},\"t\":{tools},\"p\":{payload},\"s\":\"{prompt}\",\"think\":{thinking}}"
                .to_owned(),
            ..ApiTransportConfig::default()
        };
        let messages = serde_json::json!([{"role": "user", "content": "hi"}]);
        let tools = serde_json::json!([{"type": "function"}]);
        let body = template.render_body(&messages, Some(&tools), "INSTRUCTION", true);
        let parsed: Value = serde_json::from_str(&body).expect("rendered body is JSON");
        assert_eq!(parsed["m"][0]["content"], "hi");
        assert_eq!(parsed["t"][0]["type"], "function");
        assert_eq!(parsed["p"]["messages"][0]["role"], "user");
        assert_eq!(parsed["s"], "INSTRUCTION");
        // A JSON boolean, not the string "true": the flag goes into a body.
        assert_eq!(parsed["think"], Value::Bool(true));

        // `{tools}` renders as JSON null when the turn declares none, and the
        // reasoning flag turns off for a plain chat turn.
        let body = template.render_body(&messages, None, "x", false);
        let parsed: Value = serde_json::from_str(&body).expect("rendered body is JSON");
        assert!(parsed["t"].is_null());
        assert_eq!(parsed["think"], Value::Bool(false));
    }

    #[test]
    fn the_shipped_api_body_cannot_express_reasoning_which_is_why_it_must_be_asked() {
        // The default template is deliberately generic and carries no
        // `{thinking}`. That is the case `call_api` refuses: an endpoint that
        // cannot be told which model to use must not answer as if it had been.
        let default = ApiTransportConfig::default();
        assert!(
            !default.body.contains("{thinking}"),
            "the shipped body is generic, so a pro turn over the api transport is \
             refused until the template says how to express reasoning"
        );
    }

    #[test]
    fn api_transport_accepts_static_extra_headers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            [transport]
            mode = "api"
            [transport.api]
            url = "https://example.invalid/v1/complete"
            framing = "json"
            text_path = "choices.0.message.content"
            extra_headers = { "x-client-version" = "2.5.0", "authorization" = "Bearer static" }
            "#,
        )
        .expect("write user config");

        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.transport.mode, TransportMode::Api);
        assert_eq!(
            config.transport.api.extra_headers.get("x-client-version"),
            Some(&"2.5.0".to_owned())
        );
        assert_eq!(
            config
                .transport
                .api
                .extra_headers
                .get("authorization")
                .map(String::as_str),
            Some("Bearer static")
        );
    }

    #[test]
    fn json_paths_index_objects_and_arrays() {
        let value = serde_json::json!({
            "choices": [{"delta": {"content": "hello"}}],
            "content": "flat",
        });
        assert_eq!(
            text_at_path(&value, "choices.0.delta.content").as_deref(),
            Some("hello")
        );
        assert_eq!(text_at_path(&value, "content").as_deref(), Some("flat"));
        assert_eq!(text_at_path(&value, "choices.9.content"), None);
        // An empty path means the whole value is the text.
        assert_eq!(
            text_at_path(&Value::String("raw".to_owned()), "").as_deref(),
            Some("raw")
        );
        assert_eq!(text_at_path(&value, ""), None);
    }

    #[test]
    fn user_config_overrides_only_named_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r##"
            [chat]
            url = "https://chat.deepseek.com/custom"
            [selectors]
            composer = "#my-composer"
            [codewhale]
            binary = "/opt/codewhale"
            "##,
        )
        .expect("write user config");

        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.chat.url, "https://chat.deepseek.com/custom");
        assert_eq!(config.selectors.composer, "#my-composer");
        // Untouched values keep their committed defaults.
        assert!(config.selectors.assistant.contains("ds-markdown"));
        assert_eq!(
            config.chat.allowed_hosts,
            vec!["chat.deepseek.com".to_owned()]
        );
        assert_eq!(config.codewhale.binary.as_deref(), Some("/opt/codewhale"));
    }

    #[test]
    fn missing_user_config_falls_back_to_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let config = Config::load(Some(&missing)).expect("load");
        assert_eq!(config.chat.url, Config::defaults().chat.url);
    }

    #[test]
    fn url_acceptance_is_config_driven() {
        let config = Config::defaults();
        assert!(config.chat.accepts_url("https://chat.deepseek.com/"));
        assert!(!config.chat.accepts_url("http://chat.deepseek.com/"));
        assert!(!config.chat.accepts_url("https://deepseek.ai/chat"));
        assert!(
            !config
                .chat
                .accepts_url("https://chat.deepseek.com.attacker.invalid/")
        );
        assert!(!config.chat.accepts_url("https://user@chat.deepseek.com/"));
    }

    #[test]
    fn resumable_url_requires_a_routed_path() {
        let config = Config::defaults();
        assert!(!config.chat.is_resumable_url("https://chat.deepseek.com"));
        assert!(
            config
                .chat
                .is_resumable_url("https://chat.deepseek.com/a/chat/s/abc123")
        );
    }

    #[test]
    fn remember_binary_creates_and_preserves_other_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[chat]\nurl = \"https://chat.deepseek.com/x\"\n").expect("seed");

        remember_binary(&path, Path::new("/opt/codewhale")).expect("remember");
        let config = Config::load(Some(&path)).expect("load");
        assert_eq!(config.codewhale.binary.as_deref(), Some("/opt/codewhale"));
        assert_eq!(config.chat.url, "https://chat.deepseek.com/x");

        // Idempotent when the value is unchanged.
        remember_binary(&path, Path::new("/opt/codewhale")).expect("remember again");
        assert_eq!(
            Config::load(Some(&path))
                .expect("load")
                .codewhale
                .binary
                .as_deref(),
            Some("/opt/codewhale")
        );
    }

    #[test]
    fn remember_binary_creates_the_config_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("freechatcode/config.toml");
        remember_binary(&path, Path::new("/usr/bin/codewhale")).expect("remember");
        assert_eq!(
            Config::load(Some(&path))
                .expect("load")
                .codewhale
                .binary
                .as_deref(),
            Some("/usr/bin/codewhale")
        );
    }
}
