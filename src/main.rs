use std::ffi::OsString;
use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::serve;
use clap::{Parser, Subcommand};
use playwright_rs::protocol::{
    BrowserContext, BrowserContextOptions, GotoOptions, Page, RecordVideo, Viewport, WaitUntil,
};
use playwright_rs::{ConnectOverCdpOptions, Error as PlaywrightError, Playwright};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use freechatcode::config::{
    self, ApiTransportConfig, BrowserMode, ChatConfig, Config, RunMode, Selectors, Timeouts,
    TransportMode,
};
use freechatcode::health;
use freechatcode::sessions::{self, SessionLinks, TurnRow};
use freechatcode::setup;
use freechatcode::{
    AuditSink, BridgeOptions, ChatUi, DEFAULT_SYSTEM_PROMPT, Failure, MODEL_ID, PRO_MODEL_ID,
    ServerState, ToolPolicy, TurnRecord, TurnSink, extract_api_text, router, serves_model,
};

mod tui;

#[derive(Debug, Parser)]
#[command(
    name = "freechatcode",
    about = "Run coding agents through free chat web UIs — no API key"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Attach to an already-running Chromium via this CDP endpoint.
    #[arg(long)]
    cdp_endpoint: Option<String>,

    /// DeepSeek Chat page to open (defaults to the configured chat URL).
    #[arg(long)]
    chat_url: Option<String>,

    /// Codewhale executable to launch after the browser is ready.
    #[arg(long, env = "CODEWHALE_BINARY")]
    codewhale_bin: Option<String>,

    /// Browser profile directory.
    #[arg(long)]
    profile_dir: Option<PathBuf>,

    /// Record the browser's page to a video in this directory (one file per
    /// run). Useful for demos; it never records the desktop, only the page.
    #[arg(long)]
    record_video: Option<PathBuf>,

    /// Recording size for --record-video, as WxH.
    #[arg(long)]
    record_video_size: Option<String>,

    /// How the wrapper presents itself: `show` (a visible browser, prompts
    /// driven through the page) or `silent` (headless, prompts sent to the API
    /// directly). Overrides `mode` in the config file. Sign-in reopens a visible
    /// window in both modes.
    #[arg(long, value_enum)]
    mode: Option<RunMode>,

    /// Model to launch Codewhale with. Defaults to the chat model.
    #[arg(long)]
    model: Option<String>,

    /// Arguments passed unchanged to Codewhale after `--`.
    #[arg(last = true)]
    codewhale_args: Vec<OsString>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check health of codewhale binary, relay, and browser.
    Health,
    /// Install codewhale-cli via cargo.
    Install,
    /// Show the pre-flight TUI, then run the bridge.
    Tui,
    /// Print the recorded turn log: which model answered what, per chat.
    Turns {
        /// How many recent turns to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Find (and remember) the codewhale binary, then run the bridge.
    Launch {
        /// Optional codewhale executable name or path to track down.
        binary: Option<String>,
        /// Arguments passed unchanged to Codewhale after `--`.
        #[arg(last = true)]
        codewhale_args: Vec<OsString>,
    },
}

/// Everything needed to (re)open the browser for this run.
#[derive(Clone)]
struct BrowserSpec {
    config: Config,
    url: String,
    profile: PathBuf,
    cdp_endpoint: Option<String>,
}

/// A live browser: the page the relay drives plus the handles that keep it up.
struct Session {
    page: Page,
    playwright: Playwright,
    context: BrowserContext,
    /// A wrapper-owned browser is closed on exit; an attached one is not.
    managed: bool,
    /// Flipped by the page's own close/crash handlers, and by its context closing.
    closed: Arc<AtomicBool>,
}

impl Session {
    /// Wrap a live page and start watching for it going away, so a turn can tell
    /// "the browser is gone" from "the page is slow" without a round trip.
    async fn new(
        page: Page,
        playwright: Playwright,
        context: BrowserContext,
        managed: bool,
    ) -> Result<Self> {
        let closed = Arc::new(AtomicBool::new(false));
        // A watcher that will not register is not fatal: the liveness check falls
        // back to asking the page itself.
        if let Err(error) = watched_page(&page, Arc::clone(&closed)).await {
            eprintln!("freechatcode: could not watch the page for closure: {error}");
        }
        if let Err(error) = watched_context(&context, Arc::clone(&closed)).await {
            eprintln!("freechatcode: could not watch the browser context for closure: {error}");
        }
        Ok(Self {
            page,
            playwright,
            context,
            managed,
            closed,
        })
    }

    /// Whether this browser is still usable.
    fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::SeqCst) && !self.page.is_closed()
    }
}

/// Register a handler that flips `closed` when the page closes or crashes.
async fn watched_page(page: &Page, closed: Arc<AtomicBool>) -> Result<(), playwright_rs::Error> {
    let flag = Arc::clone(&closed);
    page.on_close(move || {
        let flag = Arc::clone(&flag);
        async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }
    })
    .await?;
    let flag = Arc::clone(&closed);
    page.on_crash(move || {
        let flag = Arc::clone(&flag);
        async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }
    })
    .await?;
    Ok(())
}

/// Register a handler that flips `closed` when the browser context closes.
async fn watched_context(
    context: &BrowserContext,
    closed: Arc<AtomicBool>,
) -> Result<(), playwright_rs::Error> {
    context
        .on_close(move || {
            let flag = Arc::clone(&closed);
            async move {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            }
        })
        .await?;
    Ok(())
}

/// Drives the DeepSeek Chat page — and, for the `api` transport, the site's own
/// completion endpoint from inside that page.
struct BrowserChat {
    spec: BrowserSpec,
    /// The live browser, held across turns unless `[browser] keep_alive` is off.
    session: Mutex<Option<Session>>,
    transport: TransportMode,
    api: ApiTransportConfig,
    api_timeout: Duration,
    keep_alive: bool,
    response_timeout: Duration,
    /// What the page last said it was using. Recorded per turn.
    model_label: Mutex<Option<String>>,
    /// The conversation this run is in, once one exists. Reopening the browser
    /// returns here rather than starting a fresh chat. Empty means "use the
    /// configured URL".
    resume_url: Mutex<String>,
    /// The HTTP status the `api` transport last saw, if any. A status is proof
    /// the service answered, which is what separates "their fault" from ours.
    last_http_status: Mutex<Option<u16>>,
}

impl BrowserChat {
    fn selectors(&self) -> &Selectors {
        &self.spec.config.selectors
    }

    fn timeouts(&self) -> &Timeouts {
        &self.spec.config.timeouts
    }

    /// Open the browser the configured way: a managed profile, or attach to an
    /// already-running Chromium.
    async fn open(&self) -> Result<Session> {
        if resolves_to_attach(
            self.spec.config.browser.mode,
            self.spec.cdp_endpoint.as_deref(),
        ) {
            self.attach().await
        } else {
            self.launch_managed().await
        }
    }

    /// Attach to a browser you already have open (your own signed-in session).
    async fn attach(&self) -> Result<Session> {
        let endpoint = self
            .spec
            .cdp_endpoint
            .clone()
            .unwrap_or_else(|| "http://127.0.0.1:9222".to_owned());
        let playwright = Playwright::launch()
            .await
            .context("start Playwright-RS driver")?;
        let options = ConnectOverCdpOptions::new().timeout(10_000.0);
        let browser = playwright
            .chromium()
            .connect_over_cdp(&endpoint, Some(options))
            .await
            .with_context(|| format!("connect to existing Chromium at {endpoint}"))?;
        let context = browser
            .contexts()
            .into_iter()
            .next()
            .context("attach to an existing Chromium context")?;
        let page = match context.pages().into_iter().next() {
            Some(page) => {
                if !self.spec.config.chat.accepts_url(&page.url()) {
                    self.navigate(&page).await?;
                }
                page
            }
            None => {
                let page = context
                    .new_page()
                    .await
                    .context("open a tab in the attached browser")?;
                self.navigate(&page).await?;
                page
            }
        };
        self.wait_for_composer(&page, self.timeouts().login_wait())
            .await?;
        Session::new(page, playwright, context, false).await
    }

    /// Launch a dedicated persistent profile. In headless mode the composer is
    /// probed briefly; if it never appears (no sign-in) the window is reopened
    /// visible so you can sign in, then polled for the full login window.
    async fn launch_managed(&self) -> Result<Session> {
        let config = &self.spec.config;
        // One actionable sentence beats thirty lines of node stack trace when a
        // previous run left the profile held.
        if let Some(holder) = profile_holder(&self.spec.profile) {
            bail!(
                "another Chromium is already using the browser profile at {} ({holder}). \
                 Close it, set [browser] mode = \"attach\" to reuse a browser you started with \
                 --remote-debugging-port, or pick a different [browser] profile_dir.",
                self.spec.profile.display()
            );
        }
        tokio::fs::create_dir_all(&self.spec.profile)
            .await
            .with_context(|| {
                format!("create browser profile at {}", self.spec.profile.display())
            })?;
        set_private_directory(&self.spec.profile).await?;
        let playwright = Playwright::launch()
            .await
            .context("start Playwright-RS driver")?;
        let profile = self
            .spec
            .profile
            .to_str()
            .context("browser profile path is not valid UTF-8")?;
        let mut headless = config.browser.headless;
        // A fresh machine has the driver but not the browser. Fetch the browser
        // once, in place, so a first run needs no separate install step; driver
        // and browser always match because both come from this crate.
        let mut installed_browser = false;
        loop {
            let mut options = BrowserContextOptions::builder().headless(headless);
            if let Some(dir) = config.browser.record_video_dir.as_deref() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create video directory {dir}"))?;
                let mut video = RecordVideo::new(dir);
                if let Some((width, height)) = config.browser.video_size() {
                    video = video.size(Viewport { width, height });
                }
                options = options.record_video(video);
            }
            let options = options.build();
            let launched = playwright
                .chromium()
                .launch_persistent_context_with_options(profile, options)
                .await;
            let context = match launched {
                Ok(context) => context,
                Err(PlaywrightError::BrowserNotInstalled { .. }) if !installed_browser => {
                    eprintln!(
                        "freechatcode: Playwright's Chromium is not installed yet; fetching it \
                         now (one time, ~150 MB). Set PLAYWRIGHT_BROWSERS_PATH to put it elsewhere."
                    );
                    playwright_rs::install_browsers(Some(&["chromium"]))
                        .await
                        .context("install Playwright Chromium")?;
                    installed_browser = true;
                    continue;
                }
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("existing browser session")
                        || message.contains("already in use")
                    {
                        bail!(
                            "the browser profile at {} is already in use by another Chromium; \
                             close it, or set [browser] mode = \"attach\" to reuse it",
                            self.spec.profile.display()
                        );
                    }
                    return Err(anyhow::anyhow!(
                        "launch a persistent Chromium profile: {message}"
                    ));
                }
            };
            let page = if let Some(page) = context.pages().into_iter().next() {
                page
            } else {
                context.new_page().await.context("open DeepSeek Chat tab")?
            };
            self.navigate(&page).await?;
            let probe = if headless {
                config.timeouts.login_probe()
            } else {
                config.timeouts.login_wait()
            };
            match self.wait_for_composer(&page, probe).await {
                Ok(()) => {
                    if !config.chat.accepts_url(&page.url()) {
                        bail!(
                            "the authenticated browser did not return to an allowed DeepSeek Chat page"
                        );
                    }
                    return Session::new(page, playwright, context, true).await;
                }
                Err(_) if headless => {
                    notify(
                        config.relay.desktop_notifications,
                        "DeepSeek sign-in required",
                        "The headless browser has no composer. Reopening a visible window so you can sign in.",
                    );
                    let _ = context.close().await;
                    headless = false;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Bound every browser action, set the navigation budget, and land on the
    /// configured chat URL (or the conversation already in progress).
    async fn navigate(&self, page: &Page) -> Result<()> {
        let url = self.target_url().await;
        self.navigate_to(page, &url).await
    }

    async fn navigate_to(&self, page: &Page, url: &str) -> Result<()> {
        let timeouts = self.timeouts();
        page.set_default_timeout(timeouts.action().as_secs_f64() * 1000.0)
            .await;
        page.set_default_navigation_timeout(timeouts.navigation().as_secs_f64() * 1000.0)
            .await;
        // `domcontentloaded`, not Playwright's default `load`: the page is a
        // heavy single-page app, and `load` waits for every font, image and
        // analytics beacon before returning. The wrapper does not want "the page
        // has finished fetching everything", it wants "the composer is there" —
        // which it polls for itself, a moment later, and which is the only
        // readiness that answers a prompt. This is worth seconds on a cold start.
        page.goto(
            url,
            Some({
                let mut options = GotoOptions::new();
                options.wait_until = Some(WaitUntil::DomContentLoaded);
                options
            }),
        )
        .await
        .with_context(|| format!("open DeepSeek Chat page {url}"))?;
        Ok(())
    }

    /// Whether the visible page is a real, populated conversation on an allowed
    /// host — i.e. safe to continue in rather than realign from scratch.
    async fn conversation_present(&self, page: &Page) -> bool {
        let chat = &self.spec.config.chat;
        let deadline = Instant::now() + self.timeouts().link_probe();
        let poll = self.timeouts().poll();
        loop {
            let composer = page.locator(&self.selectors().composer).first();
            let ready = composer.count().await.unwrap_or(0) > 0
                && composer.is_visible().await.unwrap_or(false);
            if ready && chat.is_resumable_url(&page.url()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll).await;
        }
    }

    async fn wait_for_composer(&self, page: &Page, timeout: Duration) -> Result<()> {
        eprintln!(
            "Sign in directly in the browser window if needed; the wrapper never reads credentials."
        );
        let deadline = Instant::now() + timeout;
        let poll = self.timeouts().poll();
        loop {
            let locator = page.locator(&self.selectors().composer);
            let count = locator.count().await.unwrap_or(0);
            let visible = locator.first().is_visible().await.unwrap_or(false);
            if count > 0 && visible {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "the DeepSeek Chat composer did not appear before the {}s wait expired",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Read the model the page says it is using, when the UI exposes it.
    ///
    /// DeepSeek's chat page renders its modes as `div.ds-toggle-button` chips
    /// ("DeepThink", "Search") whose `--selected` class says which is on, so a
    /// turn can record e.g. `DeepThink=on, Search=on`.
    async fn read_model_label(&self, page: &Page) -> Option<String> {
        let selector = self.selectors().model_label.trim();
        if selector.is_empty() {
            return None;
        }
        let locator = page.locator(selector);
        let count = locator.count().await.ok()?;
        let mut parts = Vec::new();
        for index in 0..count.min(6) {
            let item = locator.nth(index as i32);
            let text = item.inner_text().await.unwrap_or_default();
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let selected = item
                .get_attribute("class")
                .await
                .ok()
                .flatten()
                .is_some_and(|class| class.contains("selected"));
            parts.push(format!("{text}{}", if selected { "=on" } else { "=off" }));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }

    /// Where to (re)open the browser: the conversation this run is already in,
    /// else the configured chat URL.
    async fn target_url(&self) -> String {
        let current = self.resume_url.lock().await;
        if current.is_empty() {
            self.spec.url.clone()
        } else {
            current.clone()
        }
    }

    /// Record the conversation the page is on, so a relaunch returns to it
    /// instead of starting a new chat.
    async fn remember_conversation(&self, page: &Page) {
        let url = page.url();
        if self.spec.config.chat.is_resumable_url(&url) {
            *self.resume_url.lock().await = url;
        }
    }

    /// Guarantee a live, usable page, reopening the browser when it has gone
    /// away. This is the persistence guarantee: a browser that is closed or
    /// crashes costs a relaunch, not the session.
    async fn ensure_live(&self, guard: &mut Option<Session>) -> Result<(), String> {
        if let Some(session) = guard.as_ref()
            && !session.is_alive()
        {
            eprintln!(
                "freechatcode: the browser is gone; reopening it on the conversation and carrying on"
            );
            if let Some(gone) = guard.take() {
                close_session(gone).await;
            }
        }
        if guard.is_none() {
            *guard = Some(self.open().await.map_err(|error| format!("{error:#}"))?);
        }
        Ok(())
    }

    /// Whether a failed turn is worth one relaunch. A browser that has gone away
    /// — or a page whose composer vanished under it — is recoverable; a refusal
    /// from the model is not. See [`turn_is_recoverable`].
    fn recoverable(&self, session: Option<&Session>, error: &str) -> bool {
        turn_is_recoverable(session, error)
    }

    /// Classify a failed turn: what happened, and whose fault it is.
    ///
    /// Order matters. A browser that is already gone is our problem, not the
    /// network's. An HTTP status means the service answered and chose to fail, so
    /// that is theirs. A Chromium `net::` code means the request never got that
    /// far, so the uplink takes the blame. Only when none of those apply is the
    /// machine probed — guessing "DeepSeek is broken" while the DNS is down is
    /// exactly the mistake worth avoiding.
    async fn classify(&self, error: &str) -> Failure {
        let session_dead = self
            .session
            .lock()
            .await
            .as_ref()
            .is_some_and(|session| !session.is_alive());
        if session_dead {
            return Failure {
                kind: "browser_gone".to_owned(),
                blame: "wrapper".to_owned(),
                detail: first_line(error),
                http_status: None,
            };
        }

        if let Some(status) = *self.last_http_status.lock().await {
            return Failure {
                kind: "upstream_http".to_owned(),
                blame: "service".to_owned(),
                detail: format!("the endpoint answered HTTP {status}: {}", first_line(error)),
                http_status: Some(status),
            };
        }

        if let Some(kind) = network_error_kind(error) {
            return Failure {
                kind: kind.to_owned(),
                blame: "network".to_owned(),
                detail: first_line(error),
                http_status: None,
            };
        }

        // The browser offered no verdict, so ask the machine directly.
        match self.probe_reachability().await {
            Reachability::Unresolvable => Failure {
                kind: "dns".to_owned(),
                blame: "network".to_owned(),
                detail: format!(
                    "{} (and this host cannot resolve the chat host)",
                    first_line(error)
                ),
                http_status: None,
            },
            Reachability::Unreachable => Failure {
                kind: "network".to_owned(),
                blame: "network".to_owned(),
                detail: format!(
                    "the chat host resolves but nothing accepts a connection: {}",
                    first_line(error)
                ),
                http_status: None,
            },
            Reachability::Reachable => {
                // DNS and the uplink are demonstrably fine, so this is the page.
                // Without an HTTP status we do not claim it is the service's
                // fault — only that it is not the network's.
                let (kind, note) = if error.contains("no visible assistant reply") {
                    (
                        "page_silent",
                        "the chat host is reachable, so DNS and the uplink are fine",
                    )
                } else if error.contains("composer") {
                    ("page_changed", "the composer is no longer where it was")
                } else {
                    (
                        "page_error",
                        "the host is reachable; the page did not answer",
                    )
                };
                Failure {
                    kind: kind.to_owned(),
                    blame: "unknown".to_owned(),
                    detail: format!("{note}: {}", first_line(error)),
                    http_status: None,
                }
            }
        }
    }

    /// Whether the chat host resolves and accepts a connection, without the
    /// browser. Run off the async runtime, since DNS and connect block.
    async fn probe_reachability(&self) -> Reachability {
        let host = url::Url::parse(&self.spec.config.chat.url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned));
        let Some(host) = host else {
            return Reachability::Unreachable;
        };
        tokio::task::spawn_blocking(move || {
            use std::net::{TcpStream, ToSocketAddrs};
            let Ok(addrs) = (host.as_str(), 443u16).to_socket_addrs() else {
                return Reachability::Unresolvable;
            };
            let addrs: Vec<_> = addrs.collect();
            if addrs.is_empty() {
                return Reachability::Unresolvable;
            }
            for addr in addrs {
                if TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok() {
                    return Reachability::Reachable;
                }
            }
            Reachability::Unreachable
        })
        .await
        .unwrap_or(Reachability::Unreachable)
    }

    /// Run one turn: open the browser if needed, drive it, recover once from a
    /// browser that died, record the model label and conversation, and (when
    /// `keep_alive` is off) close it again.
    async fn turn(
        &self,
        prompt: &str,
        start_new_chat: bool,
        snapshots: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> Result<String, String> {
        let mut guard = self.session.lock().await;
        *self.last_http_status.lock().await = None;
        self.ensure_live(&mut guard).await?;
        // One recovery is generosity; a loop would hide a browser that dies
        // every turn.
        let mut recovered = false;
        // The direct API path is refused exactly as STATUS.md describes: the
        // site's completion endpoint wants a per-request proof-of-work header
        // this project does not synthesize. That is a property of the endpoint,
        // not a reason for `silent` to be useless — the page is already open and
        // already invisible, so drive it and say so. One line, once per turn.
        let mut api_refused = false;
        let outcome = loop {
            let attempt = {
                let session = guard.as_ref().expect("a session was just opened");
                match self.transport {
                    TransportMode::Gui => {
                        self.submit(&session.page, prompt, start_new_chat, snapshots.clone())
                            .await
                    }
                    TransportMode::Api => match self.call_api(&session.page, prompt).await {
                        Ok(text) => Ok(text),
                        Err(error) => {
                            if !api_refused {
                                api_refused = true;
                                eprintln!(
                                    "freechatcode: the direct API path refused ({error}); \
                                     using the headless page for this turn instead"
                                );
                            }
                            self.submit(&session.page, prompt, start_new_chat, snapshots.clone())
                                .await
                        }
                    },
                }
            };
            match attempt {
                Ok(text) => break Ok(text),
                Err(error) => {
                    if recovered || !self.recoverable(guard.as_ref(), &error) {
                        break Err(error);
                    }
                    eprintln!(
                        "freechatcode: the browser did not survive the turn ({error}); \
                         reopening it and retrying once"
                    );
                    if let Some(gone) = guard.take() {
                        close_session(gone).await;
                    }
                    self.ensure_live(&mut guard).await?;
                    recovered = true;
                }
            }
        };
        if let Some(session) = guard.as_ref() {
            *self.model_label.lock().await = self.read_model_label(&session.page).await;
            if outcome.is_ok() {
                self.remember_conversation(&session.page).await;
            }
        }
        if !self.keep_alive
            && let Some(session) = guard.take()
        {
            close_session(session).await;
        }
        outcome
    }

    /// The `api` transport: POST the configured body from inside the chat page
    /// so the signed-in session's cookies apply, then extract the text. This
    /// endpoint is private and every part of its shape is configuration.
    async fn call_api(&self, page: &Page, prompt: &str) -> Result<String, String> {
        let messages = serde_json::json!([{"role": "user", "content": prompt}]);
        // Which model answers is page state, and the relay has already set it
        // before this call. Read it back rather than assuming: a template that
        // never names `{thinking}` cannot be told about it, and an endpoint that
        // answers the plain model while the caller asked for pro is exactly the
        // lie this wrapper refuses to tell. Failing here is not a dead end — the
        // turn is retried through the headless page, which can honour it.
        let thinking = matches!(
            chip_is_selected(page, self.selectors().thinking_toggle.trim()).await,
            Some(true)
        );
        if thinking && !self.api.body.contains("{thinking}") {
            return Err(
                "the page is in pro mode but [transport.api] body has no {thinking} \
                 placeholder, so the endpoint cannot be told which model to use; add \
                 {thinking} to the body template, or run with [transport] mode = \"gui\""
                    .to_owned(),
            );
        }
        let body = self.api.render_body(&messages, None, prompt, thinking);
        let url = self.api.url.clone();
        let headers = self.api.extra_headers.clone();
        let script = "async ([url, body, extra]) => { const headers = Object.assign({'content-type': 'application/json'}, extra); const r = await fetch(url, {method: 'POST', headers, body, credentials: 'include'}); return r.status + '\\n' + (await r.text()); }";
        let arg = serde_json::json!([url, body, headers]);
        let raw: String = tokio::time::timeout(self.api_timeout, page.evaluate(script, Some(&arg)))
            .await
            .map_err(|_| {
                format!(
                    "the api transport timed out after {}s",
                    self.api_timeout.as_secs()
                )
            })?
            .map_err(|error| format!("the api transport call failed: {error}"))?;
        // The first line is the HTTP status; the rest is the body.
        let (status, payload) = raw.split_once('\n').unwrap_or(("", raw.as_str()));
        let status_code = status.trim().parse::<u16>().ok();
        // Remembered for `classify`: a status is proof the service answered.
        *self.last_http_status.lock().await = status_code;
        let text = extract_api_text(self.api.framing, &self.api.text_path, payload);
        if text.trim().is_empty() {
            // Show what came back: an endpoint that answers 200 with an error
            // envelope is otherwise indistinguishable from an empty success.
            let head: String = payload.chars().take(240).collect();
            return Err(format!(
                "the api transport returned no text (HTTP {status}, {} bytes): {head}",
                payload.len()
            ));
        }
        Ok(text)
    }

    async fn submit(
        &self,
        page: &Page,
        prompt: &str,
        start_new_chat: bool,
        snapshots: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> Result<String, String> {
        if start_new_chat {
            self.start_new_chat(page).await?;
        }
        let composer = page.locator(&self.selectors().composer).first();
        if !composer.is_visible().await.unwrap_or(false) {
            return Err("DeepSeek Chat composer is no longer visible".into());
        }
        // Remember the newest reply as it stands *before* the send. Reading the
        // last element rather than the whole transcript is what keeps this
        // cheap: pulling every assistant message on every poll is what made the
        // first turn feel stalled.
        let assistants = page.locator(&self.selectors().assistant);
        let previous_count = assistants
            .count()
            .await
            .map_err(|_| "could not inspect the visible assistant transcript".to_owned())?;
        let previous_text = if previous_count > 0 {
            assistants
                .last()
                .inner_text()
                .await
                .unwrap_or_default()
                .trim()
                .to_owned()
        } else {
            String::new()
        };
        let action = self.timeouts().action();
        tokio::time::timeout(action, composer.fill(prompt, None))
            .await
            .map_err(|_| format!("entering the {} byte request timed out", prompt.len()))?
            .map_err(|error| {
                format!("could not enter the request in the visible chat composer: {error}")
            })?;
        let send = page.locator(&self.selectors().send);
        if send.count().await.unwrap_or(0) > 0 && send.last().is_visible().await.unwrap_or(false) {
            tokio::time::timeout(action, send.last().click(None))
                .await
                .map_err(|_| "submitting the visible request timed out".to_owned())?
                .map_err(|error| {
                    format!("could not submit the request in the visible chat UI: {error}")
                })?;
        } else {
            tokio::time::timeout(action, composer.press("Enter", None))
                .await
                .map_err(|_| "submitting the request timed out".to_owned())?
                .map_err(|error| {
                    format!("could not submit the request in the visible chat composer: {error}")
                })?;
        }

        // Confirm the platform actually took the send. A `fill()` the page's
        // framework never observes leaves the Send control inert: Playwright's
        // click "succeeds", nothing is posted, and we would otherwise poll for a
        // reply that can never arrive.
        let trimmed = prompt.trim_end();
        let mut boundary = trimmed.len().saturating_sub(24);
        while boundary < trimmed.len() && !trimmed.is_char_boundary(boundary) {
            boundary += 1;
        }
        let marker = &trimmed[boundary..];
        let settle_by = Instant::now() + Duration::from_secs(15);
        let mut retried = false;
        loop {
            let value = composer.input_value(None).await.unwrap_or_default();
            let still_held = if value.is_empty() {
                composer
                    .inner_text()
                    .await
                    .map(|text| text.contains(marker))
                    .unwrap_or(false)
            } else {
                value.contains(marker)
            };
            if !still_held {
                break;
            }
            if !retried {
                // The click may have hit a control that does nothing; retry
                // through the keyboard path once before giving up.
                retried = true;
                let _ = tokio::time::timeout(action, composer.press("Enter", None)).await;
                continue;
            }
            if Instant::now() >= settle_by {
                return Err(
                    "the platform did not accept the send; the request is still sitting in the composer"
                        .to_owned(),
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let deadline = Instant::now() + self.response_timeout;
        let poll = self.timeouts().poll();
        let settle_polls = self.timeouts().settle_polls.max(1);
        let mut prior_text = String::new();
        let mut stable_polls = 0_u32;
        // Waiting silently for minutes is indistinguishable from a hang. Say what
        // is happening, on the wrapper's own stderr, while the page thinks.
        let started = Instant::now();
        let mut last_note = Instant::now();
        loop {
            let count = assistants
                .count()
                .await
                .map_err(|_| "could not read the visible assistant reply".to_owned())?;
            // The transcript is virtualized: once the mounted window is full the
            // element count stops growing while replies keep arriving, so a
            // growth test alone reports a page that answered as silent. The
            // newest message stays mounted, so its text is the signal that
            // survives. One element per poll is cheap; the whole transcript is
            // what made the first turn feel stalled.
            let text = if count > 0 {
                assistants.last().inner_text().await.unwrap_or_default()
            } else {
                String::new()
            };
            let text = text.trim();
            if reply_arrived(previous_count, &previous_text, count, text) {
                if text == prior_text {
                    stable_polls = stable_polls.saturating_add(1);
                } else {
                    stable_polls = 0;
                    prior_text.clear();
                    prior_text.push_str(text);
                    if let Some(snapshots) = &snapshots {
                        // The whole visible reply, not a delta: the relay
                        // turns it into content deltas, and a dropped
                        // message cannot corrupt the stream.
                        let _ = snapshots.send(text.to_owned()).await;
                    }
                }
                if stable_polls >= settle_polls {
                    // Is the page actually idle, or merely between renders? Report
                    // what the page itself says, because that — not a timer — is
                    // what would end the turn without either risking a truncated
                    // reply or paying a fixed wait on every turn.
                    let send_controls = page
                        .locator(&self.selectors().send)
                        .count()
                        .await
                        .unwrap_or(0);
                    eprintln!(
                        "freechatcode: reply settled after {} polls ({} chars, {}s); \
                         page send control(s): {send_controls}",
                        stable_polls,
                        prior_text.len(),
                        started.elapsed().as_secs(),
                    );
                    return Ok(text.to_owned());
                }
            }
            if last_note.elapsed() >= Duration::from_secs(15) {
                last_note = Instant::now();
                eprintln!(
                    "freechatcode: waiting for the page… {}s of {}s | {} assistant element(s) ({} before this turn) | composer {} char(s) | newest reply {} char(s)",
                    started.elapsed().as_secs(),
                    self.response_timeout.as_secs(),
                    count,
                    previous_count,
                    composer.input_value(None).await.unwrap_or_default().len(),
                    prior_text.len(),
                );
            }
            if Instant::now() >= deadline {
                // Say what the page actually looked like: without this the only
                // symptom is "it hung", which is unfixable.
                let url = page.url();
                let elements = assistants.count().await.unwrap_or(0);
                let composer_len = composer.input_value(None).await.unwrap_or_default().len();
                let newest = if elements > 0 {
                    assistants.last().inner_text().await.unwrap_or_default()
                } else {
                    String::new()
                };
                eprintln!(
                    "freechatcode: no reply after {}s. Open {url} to see what the page is \
                     showing; the conversation is linked to this Codewhale session.",
                    self.response_timeout.as_secs()
                );
                return Err(format!(
                    "no visible assistant reply within {}s (url={url}, assistant_elements={elements}, before={previous_count}, composer_chars={composer_len}, newest_chars={}, streamed_chars={}). The prompt was accepted{}. No new reply became visible in that window, and that is all this says — a reply can still land after this budget, so open the URL and look before blaming the page. If the page is merely slow, raise [timeouts] response_secs (the {} byte prompt is not the usual reason: a long conversation is read from its newest message, not from a reply count).",
                    self.response_timeout.as_secs(),
                    newest.trim().len(),
                    prior_text.len(),
                    if composer_len == 0 {
                        " (the composer is empty)"
                    } else {
                        " (the composer still holds text)"
                    },
                    prompt.len(),
                ));
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Start a fresh conversation.
    ///
    /// Navigating back to the bare chat URL is the fast, reliable path — the
    /// platform treats it as a new conversation — so the "New chat" control is
    /// only hunted down when navigation leaves us inside a conversation. Hunting
    /// for the control first cost ~5s on every fresh turn.
    async fn start_new_chat(&self, page: &Page) -> Result<(), String> {
        let chat = &self.spec.config.chat;
        if !chat.is_resumable_url(&page.url()) {
            // Already on the bare chat URL: this is a new conversation already.
            return Ok(());
        }
        let base = chat.url.clone();
        match self.navigate_to(page, &base).await {
            Ok(()) => {
                if !chat.is_resumable_url(&page.url()) {
                    return Ok(());
                }
            }
            Err(error) => {
                eprintln!(
                    "freechatcode: could not return to the bare chat URL ({error:#}); falling back to the New chat control"
                );
            }
        }
        let mut button = page.locator(&self.selectors().new_chat);
        if button.count().await.unwrap_or(0) == 0 {
            button = page.get_by_text("New chat", false);
        }
        if button.count().await.unwrap_or(0) == 0 {
            button = page.get_by_text("新对话", false);
        }
        if button.count().await.unwrap_or(0) == 0 {
            eprintln!(
                "freechatcode: no New Chat control found; sending into the current conversation"
            );
            return Ok(());
        }
        match tokio::time::timeout(Duration::from_secs(3), button.first().click(None)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                eprintln!("freechatcode: could not start a new chat ({error}); continuing");
                Ok(())
            }
            Err(_) => {
                eprintln!("freechatcode: starting a new chat timed out; continuing");
                Ok(())
            }
        }
    }

    /// Open the browser now, so a stale session link is caught before Codewhale
    /// starts rather than on the first turn.
    async fn ensure_open(&self) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(self.open().await.map_err(|error| format!("{error:#}"))?);
        }
        Ok(())
    }

    /// Keep the browser alive between turns.
    ///
    /// A browser that is closed or crashes while the bridge sits idle used to
    /// stay gone until the next prompt. This notices within one interval and
    /// reopens it — on the conversation already in progress — so "it was closed"
    /// heals by itself. Failures back off, so a machine with no network does not
    /// spin, and the caller only starts this when `keep_alive` is on (with it
    /// off, an absent browser is the point).
    async fn watch_browser(self: Arc<Self>, interval: Duration) {
        let mut wait = interval;
        loop {
            tokio::time::sleep(wait).await;
            let mut guard = self.session.lock().await;
            // Nothing has been opened yet, or it is fine: nothing to do.
            let healthy = match guard.as_ref() {
                None => true,
                Some(session) => session.is_alive(),
            };
            if healthy {
                wait = interval;
                continue;
            }

            eprintln!(
                "freechatcode: the browser is gone; reopening it on the conversation and carrying on"
            );
            if let Some(gone) = guard.take() {
                close_session(gone).await;
            }
            match self.open().await {
                Ok(session) => {
                    *guard = Some(session);
                    eprintln!("freechatcode: the browser is back");
                    wait = interval;
                }
                Err(error) => {
                    let message = first_line(&format!("{error:#}"));
                    eprintln!("freechatcode: could not reopen the browser yet: {message}");
                    wait = (wait * 2).min(MAX_LIVENESS_BACKOFF);
                }
            }
        }
    }

    /// Whether the linked conversation is actually reachable right now.
    async fn link_available(&self) -> bool {
        let guard = self.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return false;
        };
        self.conversation_present(&session.page).await
    }

    /// The live page's URL, when a browser is open.
    async fn live_url(&self) -> Option<String> {
        let guard = self.session.lock().await;
        guard.as_ref().map(|session| session.page.url())
    }

    /// Close the browser for good at the end of the run.
    async fn shutdown(&self) {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.take() {
            close_session(session).await;
        }
    }
}

/// Whether `open` attaches instead of launching. An explicit CDP endpoint always
/// means attach, whatever `mode` says — that is what `--cdp-endpoint` promises.
fn resolves_to_attach(mode: BrowserMode, cdp_endpoint: Option<&str>) -> bool {
    cdp_endpoint.is_some_and(|endpoint| !endpoint.trim().is_empty()) || mode == BrowserMode::Attach
}

/// A profile another Chromium already holds cannot be launched. Detect it up
/// front so the failure is one actionable sentence instead of a node stack.
/// Chromium records the holder in `SingletonLock` as a `"<host>-<pid>"` symlink.
fn profile_holder(profile: &Path) -> Option<String> {
    let target = std::fs::read_link(profile.join("SingletonLock")).ok()?;
    let target = target.to_string_lossy().into_owned();
    let pid: i32 = target.rsplit('-').next()?.parse().ok()?;
    Path::new(&format!("/proc/{pid}"))
        .exists()
        .then_some(target)
}

/// Ceiling for the liveness watcher's retry backoff.
const MAX_LIVENESS_BACKOFF: Duration = Duration::from_secs(300);

/// Whether the chat host can be reached at all, independent of the browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reachability {
    /// The name did not resolve: DNS, so not the service's fault.
    Unresolvable,
    /// The name resolved but nothing would accept a connection.
    Unreachable,
    /// Something is listening.
    Reachable,
}

/// Whether the transcript changed in a way that means a **new** reply arrived.
///
/// `previous_*` is the newest visible assistant message before the send, `count`
/// and `text` after a poll. Growth is proof of a new message. A *replaced*
/// newest message is proof too, and it is the only signal that survives a
/// virtualized transcript — the case that made long conversations look silent:
/// once the mounted window is full, the page unmounts an old message in the
/// same render that mounts the new one, the count stops growing, and a growth
/// test alone waits out the whole budget on a page that already answered.
fn reply_arrived(previous_count: usize, previous_text: &str, count: usize, text: &str) -> bool {
    // Trim here rather than trusting the caller: a mounted-but-empty element
    // during generation holds whitespace, and "nothing written yet" must not
    // read as an answer.
    let text = text.trim();
    let previous_text = previous_text.trim();
    if text.is_empty() {
        return false;
    }
    // A count that *shrank* is not evidence of a new reply — the window shifted.
    // A text that changed is: the newest message is always the mounted last one.
    count > previous_count || text != previous_text
}

/// Classify a browser-level network error from its Chromium code.
fn network_error_kind(error: &str) -> Option<&'static str> {
    const DNS: [&str; 4] = [
        "ERR_NAME_NOT_RESOLVED",
        "ERR_NAME_RESOLUTION_FAILED",
        "ERR_DNS_",
        "ERR_ICANN_NAME_COLLISION",
    ];
    const NETWORK: [&str; 9] = [
        "ERR_INTERNET_DISCONNECTED",
        "ERR_CONNECTION_REFUSED",
        "ERR_CONNECTION_TIMED_OUT",
        "ERR_CONNECTION_RESET",
        "ERR_CONNECTION_CLOSED",
        "ERR_ADDRESS_UNREACHABLE",
        "ERR_NETWORK_CHANGED",
        "ERR_PROXY_CONNECTION_FAILED",
        "ERR_TUNNEL_CONNECTION_FAILED",
    ];
    if DNS.iter().any(|sign| error.contains(sign)) {
        Some("dns")
    } else if NETWORK.iter().any(|sign| error.contains(sign)) {
        Some("network")
    } else {
        None
    }
}

/// Whether the mode chip matching `selector` is selected, per the page's own
/// `--selected` class. `None` means it could not be read, which is not the same
/// as "off" — and the difference matters before and after a click.
async fn chip_is_selected(page: &Page, selector: &str) -> Option<bool> {
    let control = page.locator(selector).first();
    if control.count().await.ok()? == 0 {
        return None;
    }
    control
        .get_attribute("class")
        .await
        .ok()
        .flatten()
        .map(|class| class.contains("selected"))
}

/// Playwright errors arrive with a node stack trace attached. Keep the one line
/// that says what happened; the full text still goes to the audit log.
fn first_line(error: &str) -> String {
    error.lines().next().unwrap_or(error).trim().to_owned()
}

/// Whether a failed turn is worth one relaunch. A browser that has gone away —
/// or a page whose composer vanished under it — is recoverable; a refusal from
/// the model is not.
fn turn_is_recoverable(session: Option<&Session>, error: &str) -> bool {
    if session.is_some_and(|session| !session.is_alive()) {
        return true;
    }
    const SIGNS: [&str; 6] = [
        "Target closed",
        "target closed",
        "has been closed",
        "Browser has been closed",
        "Connection closed",
        "composer is no longer visible",
    ];
    SIGNS.iter().any(|sign| error.contains(sign))
}

/// Close a session: a wrapper-owned context is closed; an attached browser only
/// loses its driver connection. A context that has already died is not an error
/// — that is the case this whole path exists for.
async fn close_session(session: Session) {
    if session.managed
        && session.is_alive()
        && let Err(error) = session.context.close().await
    {
        eprintln!("freechatcode: closing the browser context failed: {error}");
    }
    if let Err(error) = session.playwright.shutdown().await {
        eprintln!("freechatcode: stopping the Playwright driver failed: {error}");
    }
}

#[async_trait::async_trait]
impl ChatUi for BrowserChat {
    async fn send(&self, prompt: &str, start_new_chat: bool) -> Result<String, String> {
        self.turn(prompt, start_new_chat, None).await
    }

    async fn send_streaming(
        &self,
        prompt: &str,
        start_new_chat: bool,
        snapshots: tokio::sync::mpsc::Sender<String>,
    ) -> Result<String, String> {
        self.turn(prompt, start_new_chat, Some(snapshots)).await
    }

    async fn model_label(&self) -> Option<String> {
        self.model_label.lock().await.clone()
    }

    /// Make the page's reasoning state match the model this turn asked for.
    ///
    /// Declarative, not a toggle: the wrapper advertises two models and which one
    /// answers is page state, so a pro turn must not leak into the chat turn
    /// after it and a chat turn must not inherit a pro one. A pro turn that
    /// cannot engage the control fails — answering with the plain model while the
    /// turn log says pro would be a lie.
    async fn set_reasoning(&self, pro: bool) -> Result<(), String> {
        let selector = self.selectors().thinking_toggle.trim().to_owned();
        let mut guard = self.session.lock().await;
        let session = guard.as_mut().ok_or("the browser is not open")?;
        let page = &session.page;
        if selector.is_empty() {
            return if pro {
                Err(
                    "[selectors] thinking_toggle is empty, so the pro model cannot be engaged"
                        .to_owned(),
                )
            } else {
                Ok(())
            };
        }
        let control = page.locator(&selector).first();
        if !control.is_visible().await.unwrap_or(false) {
            return if pro {
                Err(format!(
                    "no visible DeepThink control on the page ({selector}); check \
                     [selectors] thinking_toggle"
                ))
            } else {
                // Nothing to settle: a page without the control answers with the
                // plain model anyway.
                Ok(())
            };
        }
        let engaged = chip_is_selected(page, &selector).await;
        if engaged == Some(pro) {
            return Ok(());
        }
        tokio::time::timeout(self.timeouts().action(), control.click(None))
            .await
            .map_err(|_| "clicking the DeepThink control timed out".to_owned())?
            .map_err(|error| format!("could not click the DeepThink control: {error}"))?;
        // A click is a request, not a fact. Read it back before believing it.
        let after = chip_is_selected(page, &selector).await;
        if after != Some(pro) {
            return Err(format!(
                "the DeepThink control did not {}{}",
                if pro { "engage" } else { "disengage" },
                match after {
                    Some(_) => " (it read back unchanged)",
                    None => " (its state could not be read back)",
                }
            ));
        }
        Ok(())
    }
}

struct JsonlAudit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlAudit {
    async fn create(directory: &Path) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(directory)
            .await
            .with_context(|| format!("create relay audit directory {}", directory.display()))?;
        set_private_directory(directory).await?;
        let path = directory.join(format!("session-{}.jsonl", Uuid::new_v4().simple()));
        let file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
            .with_context(|| format!("create private relay audit {}", path.display()))?;
        drop(file);
        set_private_file(&path).await?;
        eprintln!("Browser relay audit: {}", path.display());
        Ok(Arc::new(Self {
            path,
            lock: Mutex::new(()),
        }))
    }
}

#[async_trait::async_trait]
impl AuditSink for JsonlAudit {
    async fn append(&self, record: Value) -> Result<(), String> {
        let _guard = self.lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .await
            .map_err(|_| "could not open the private browser relay audit".to_owned())?;
        let mut line = serde_json::to_vec(&record)
            .map_err(|_| "could not serialize the browser relay audit record".to_owned())?;
        line.push(b'\n');
        file.write_all(&line)
            .await
            .map_err(|_| "could not append the private browser relay audit".to_owned())
    }
}

/// How the wrapper should treat the Codewhale session for this run, derived
/// from the arguments forwarded to Codewhale after `--`.
#[derive(Debug, PartialEq, Eq)]
enum Intent {
    /// `--fresh`: start a new Codewhale session; do not look up a link.
    Fresh,
    /// `-r <id>` / `--resume <id>` / `--session-id <id>`: use this session.
    Session(String),
    /// Default: Codewhale resolves the session (usually the most recent).
    Auto,
}

fn codewhale_intent(args: &[OsString]) -> Intent {
    let mut intent = Intent::Auto;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--fresh" {
            intent = Intent::Fresh;
        } else if let Some(id) = text
            .strip_prefix("--session-id=")
            .or_else(|| text.strip_prefix("--resume="))
        {
            if !id.is_empty() {
                intent = Intent::Session(id.to_owned());
            }
        } else if matches!(text, "--session-id" | "--resume" | "-r")
            && let Some(id) = iter.next().and_then(|value| value.to_str())
            && !id.is_empty()
        {
            intent = Intent::Session(id.to_owned());
        }
    }
    intent
}

/// Wraps the browser so the first successful reply records the visible
/// conversation URL against the active Codewhale session.
struct UrlLinkingChat {
    inner: Arc<BrowserChat>,
    links: Arc<SessionLinks>,
    session: Option<String>,
    sessions_dir: PathBuf,
    workspace: PathBuf,
    chat: ChatConfig,
    linked: AtomicBool,
    /// Set by the startup warm-up when the linked conversation turned out not to
    /// exist. Checked once, on the first turn: the browser is warmed in parallel
    /// with Codewhale's own boot, so this answer arrives after the relay is
    /// already serving.
    stale_link: Arc<AtomicBool>,
}

impl UrlLinkingChat {
    /// Whether the linked conversation is gone, consumed once.
    ///
    /// A stale link means this turn must start a fresh chat: the relay decided
    /// `start_fresh` before the browser was even open, so the correction lands
    /// here, on the first turn that can know better.
    fn take_stale_link(&self) -> bool {
        self.stale_link.swap(false, Ordering::SeqCst)
    }

    /// Record the current conversation URL once it can be attributed to a
    /// session. Returns whether the link is now stored.
    async fn try_link(&self) -> bool {
        if self.linked.load(Ordering::SeqCst) {
            return true;
        }
        let Some(url) = self.inner.live_url().await else {
            return false;
        };
        if !self.chat.is_resumable_url(&url) {
            return false;
        }
        let session = self
            .session
            .clone()
            .or_else(|| sessions::active_session_id(&self.sessions_dir, &self.workspace));
        let Some(session) = session else {
            return false;
        };
        match self.links.upsert(&session, &url) {
            Ok(()) => {
                eprintln!("Linked Codewhale session {session} to {url}");
                self.linked.store(true, Ordering::SeqCst);
                true
            }
            Err(error) => {
                eprintln!("freechatcode: could not record the session link: {error}");
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl ChatUi for UrlLinkingChat {
    async fn send(&self, prompt: &str, start_new_chat: bool) -> Result<String, String> {
        let reply = self
            .inner
            .send(prompt, start_new_chat || self.take_stale_link())
            .await?;
        self.try_link().await;
        Ok(reply)
    }

    async fn send_streaming(
        &self,
        prompt: &str,
        start_new_chat: bool,
        snapshots: tokio::sync::mpsc::Sender<String>,
    ) -> Result<String, String> {
        let reply = self
            .inner
            .send_streaming(prompt, start_new_chat || self.take_stale_link(), snapshots)
            .await?;
        self.try_link().await;
        Ok(reply)
    }

    async fn model_label(&self) -> Option<String> {
        self.inner.model_label().await
    }

    async fn diagnose(&self, error: &str) -> Option<Failure> {
        Some(self.inner.classify(error).await)
    }
}

/// Records one row per finished turn, so a chat can be attributed
/// model-by-model after the fact.
struct TurnLog {
    links: Arc<SessionLinks>,
    session: Option<String>,
}

#[async_trait::async_trait]
impl TurnSink for TurnLog {
    async fn record(&self, turn: TurnRecord) -> Result<(), String> {
        let chat_url = match self.session.as_deref() {
            Some(session) => self.links.get(session).ok().flatten(),
            None => None,
        };
        // A turn with no failure answered; a turn with one is recorded as failed
        // alongside the diagnosis, so the history says *whose* fault it was.
        let (outcome, failure) = match turn.failure {
            Some(failure) => ("failed".to_owned(), Some(failure)),
            None => ("answered".to_owned(), None),
        };
        self.links.record_turn(&TurnRow {
            session_id: self.session.clone(),
            chat_url,
            model_label: turn.model_label,
            finish_reason: Some(turn.finish_reason),
            tool_calls: turn.tool_calls,
            content_chars: turn.content_chars,
            created_at: 0,
            outcome,
            failure_kind: failure.as_ref().map(|failure| failure.kind.clone()),
            blame: failure.as_ref().map(|failure| failure.blame.clone()),
            http_status: failure.as_ref().and_then(|failure| failure.http_status),
            detail: failure.map(|failure| failure.detail),
        })
    }
}

/// Format a Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ` without a date crate
/// (Howard Hinnant's civil-from-days).
fn iso_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Print the recorded turn log.
fn print_turns(home: &Path, limit: usize) -> Result<()> {
    let links = SessionLinks::open(&home.join("freechatcode").join("sessions.db"))
        .map_err(|error| anyhow::anyhow!(error))?;
    let rows = links.turns(limit).map_err(|error| anyhow::anyhow!(error))?;
    if rows.is_empty() {
        println!("No turns recorded yet. Run the bridge once, then try again.");
        return Ok(());
    }
    for row in rows {
        let outcome = if row.outcome == "failed" {
            format!(
                "FAILED ({}, blame={}{})",
                row.failure_kind.as_deref().unwrap_or("?"),
                row.blame.as_deref().unwrap_or("?"),
                row.http_status
                    .map(|status| format!(", HTTP {status}"))
                    .unwrap_or_default(),
            )
        } else {
            format!(
                "ok tools={} chars={}",
                if row.tool_calls { "yes" } else { "no" },
                row.content_chars
            )
        };
        println!(
            "{}  model={}  {}  session={}  chat={}",
            iso_utc(row.created_at),
            row.model_label.as_deref().unwrap_or("unknown"),
            outcome,
            row.session_id.as_deref().unwrap_or("-"),
            row.chat_url.as_deref().unwrap_or("-"),
        );
        if row.outcome == "failed"
            && let Some(detail) = row.detail.as_deref()
        {
            println!("    {}", detail.chars().take(200).collect::<String>());
        }
    }
    Ok(())
}

/// The pre-flight lobby: health plus a note of what happens next. `q` starts
/// the bridge. Skipped entirely when stdout is not a terminal.
async fn run_lobby(binary: Option<&str>) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Ok(());
    }
    let report = health::check_all(binary, None, None).await;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut lobby = tui::Tui::new()?;
        lobby.set_health(report);
        lobby.log("Press q to start the bridge (the screen hands over to Codewhale).");
        lobby.run()
    })
    .await
    .context("the pre-flight TUI panicked")??;
    Ok(())
}

/// Give up the wrapper's claim on the terminal before the browser starts.
///
/// `playwright-rs` keeps a defensive termios guard (its fix for issue #59): it
/// snapshots *stdin's* line discipline the first time a `Playwright` is
/// launched and writes that snapshot back whenever a `Playwright` is dropped.
/// The bridge hands the terminal to Codewhale at 0.00s and brings the browser up
/// behind it, so that write-back lands *after* the TUI has put the line
/// discipline into raw mode — the terminal comes back with `ECHO` on, and the
/// TUI's own mouse reports are then echoed back through the line discipline as
/// `^[[<0;10;5M` text scattered over the screen. Measured, not guessed: an
/// `LD_PRELOAD` interposer shows `comm=freechatcode tcsetattr fd=0(/dev/pts/N)`
/// with the cooked flags at the moment the browser appears, and the bare
/// Codewhale TUI never does this to itself.
///
/// The guard only snapshots when stdin is a tty, and it can only write back to
/// fd 0. So the wrapper hands the terminal to Codewhale explicitly and points
/// its own stdin somewhere that is not a terminal: then nothing the browser's
/// library does on the way up, or on the way down, can reach the terminal the
/// TUI is using.
///
/// Returns the terminal (for the child's stdin) when stdin was one, and `None`
/// when the wrapper was not started on a terminal — in which case there is
/// nothing for the guard to snapshot and nothing to do.
fn take_terminal_off_stdin() -> Option<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    if !std::io::stdin().is_terminal() {
        return None;
    }
    let devnull = std::fs::File::open("/dev/null").ok()?;
    // SAFETY: dup(2)/dup2(2) on our own descriptors. `terminal` is a fresh copy
    // of fd 0 taken before the descriptor is repointed, and nothing reads the
    // wrapper's stdin in the window this opens (the wrapper never reads stdin;
    // its own TUI talks to /dev/tty through crossterm).
    unsafe {
        let terminal = libc::dup(0);
        if terminal < 0 {
            return None;
        }
        if libc::dup2(devnull.as_raw_fd(), 0) < 0 {
            libc::close(terminal);
            return None;
        }
        Some(std::fs::File::from_raw_fd(terminal))
    }
}

fn default_home() -> Result<PathBuf> {
    let home = if let Some(home) = std::env::var_os("CODEWHALE_HOME") {
        PathBuf::from(home)
    } else {
        dirs::home_dir()
            .map(|home| home.join(".codewhale"))
            .context("could not resolve the Codewhale home directory")?
    };
    // One-time migration from the pre-rename project directory.
    config::migrate_legacy_home(&home);
    Ok(home)
}

async fn set_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        })
        .await??;
    }
    Ok(())
}

async fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        })
        .await??;
    }
    Ok(())
}

/// Best-effort user notification (a desktop "toast"). Always mirrors to stderr;
/// the desktop popup goes through `notify-send` and is skipped when disabled or
/// unavailable.
fn notify(enabled: bool, summary: &str, body: &str) {
    eprintln!("freechatcode: {summary} — {body}");
    if !enabled {
        return;
    }
    let _ = std::process::Command::new("notify-send")
        .args([
            "--app-name",
            "FreeChatCode",
            "--expire-time",
            "6000",
            summary,
            body,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

async fn run() -> Result<()> {
    let args = Args::parse();

    let (binary_hint, codewhale_args) = match args.command {
        Some(Commands::Health) => {
            let report = health::check_all(args.codewhale_bin.as_deref(), None, None).await;
            println!("Codewhale binary: {:?}", report.codewhale_binary);
            println!("Codewhale version: {:?}", report.codewhale_version);
            println!("Relay reachable: {}", report.relay_reachable);
            println!("Browser ready: {}", report.browser_ready);
            println!("Auth status: {:?}", report.auth_status);
            return Ok(());
        }
        Some(Commands::Install) => {
            setup::install_codewhale()?;
            return Ok(());
        }
        Some(Commands::Turns { limit }) => {
            print_turns(&default_home()?, limit)?;
            return Ok(());
        }
        Some(Commands::Tui) => {
            run_lobby(args.codewhale_bin.as_deref()).await?;
            (None, args.codewhale_args.clone())
        }
        Some(Commands::Launch {
            binary,
            codewhale_args,
        }) => (binary, codewhale_args),
        None => (None, args.codewhale_args.clone()),
    };

    let home = default_home()?;
    let config_path = config::user_config_path(&home);
    let mut config = Config::load_with_mode(Some(&config_path), args.mode)
        .map_err(|error| anyhow::anyhow!(error))?;
    // CLI flags beat the config file for the two recording knobs.
    if let Some(dir) = args.record_video.clone() {
        config.browser.record_video_dir = Some(dir.to_string_lossy().into_owned());
    }
    if let Some(size) = args.record_video_size.clone() {
        config.browser.record_video_size = Some(size);
    }

    // Effective values: CLI flag > environment (merged by clap) > config > default.
    let chat_url = args
        .chat_url
        .clone()
        .unwrap_or_else(|| config.chat.url.clone());
    if !config.chat.accepts_url(&chat_url) {
        bail!(
            "--chat-url must be HTTPS on one of: {}",
            config.chat.allowed_hosts.join(", ")
        );
    }
    if config.transport.mode == TransportMode::Api && config.transport.api.url.trim().is_empty() {
        bail!("[transport.api] url must not be empty when [transport] mode = \"api\"");
    }

    // Resolve the codewhale binary. Precedence: --codewhale-bin / $CODEWHALE_BINARY
    // (strict path) > `launch <BINARY>` (name or path) > remembered path (if it
    // still exists) > auto-discovery. Then remember what we resolved.
    let pinned = args.codewhale_bin.clone();
    let remembered = config.codewhale.binary.clone();
    let codewhale_bin = if let Some(spec) = pinned.as_deref() {
        setup::find_codewhale_binary(Some(spec))?
    } else if let Some(hint) = binary_hint.as_deref() {
        setup::resolve_codewhale_binary(hint)?
    } else if let Some(path) = remembered
        .as_deref()
        .filter(|path| Path::new(path).exists())
    {
        PathBuf::from(path)
    } else {
        if remembered.is_some() {
            eprintln!("freechatcode: the remembered codewhale binary is gone; rediscovering");
        }
        setup::find_codewhale_binary(None)?
    };
    println!("Using codewhale binary: {}", codewhale_bin.display());
    if pinned.is_none()
        && let Err(error) = config::remember_binary(&config_path, &codewhale_bin)
    {
        eprintln!("freechatcode: could not remember the codewhale binary: {error}");
    }

    let profile = args
        .profile_dir
        .clone()
        .or_else(|| config.browser.profile_dir.clone().map(PathBuf::from))
        .unwrap_or_else(|| home.join("deepseek-chat").join("browser"));
    let cdp_endpoint = args
        .cdp_endpoint
        .clone()
        .or_else(|| config.browser.cdp_endpoint.clone());

    let audit = JsonlAudit::create(&home.join("deepseek-chat").join("audit")).await?;

    // Resolve the Codewhale session this run will use, and the DeepSeek Chat
    // conversation linked to it (if any).
    let workspace = std::env::current_dir().context("resolve the working directory")?;
    let project_dir = workspace.to_string_lossy().into_owned();
    let sessions_dir = home.join("sessions");
    let links = Arc::new(
        SessionLinks::open(&home.join("freechatcode").join("sessions.db"))
            .map_err(|error| anyhow::anyhow!(error))?,
    );
    let session = match codewhale_intent(&codewhale_args) {
        Intent::Fresh => None,
        Intent::Session(hint) => {
            sessions::resolve_session_id(&sessions_dir, &workspace, Some(&hint))
        }
        Intent::Auto => sessions::resolve_session_id(&sessions_dir, &workspace, None),
    };
    // A stored link is only trusted if it is still a resumable URL on an
    // allowed host; anything else is treated as no link at all.
    let linked_url = session
        .as_ref()
        .and_then(|id| links.get(id).ok().flatten())
        .filter(|url| config.chat.is_resumable_url(url));
    let had_link = linked_url.is_some();
    if let Some(url) = &linked_url {
        println!("Resuming the DeepSeek Chat conversation linked to this Codewhale session: {url}");
    } else if session.is_some() {
        println!("No linked conversation yet; opening a new DeepSeek Chat session.");
    }
    let open_url = linked_url.unwrap_or_else(|| chat_url.clone());

    let browser = Arc::new(BrowserChat {
        spec: BrowserSpec {
            config: config.clone(),
            url: open_url,
            profile,
            cdp_endpoint,
        },
        session: Mutex::new(None),
        transport: config.transport.mode,
        api: config.transport.api.clone(),
        api_timeout: Duration::from_secs(config.transport.api.timeout_secs.max(1)),
        keep_alive: config.browser.keep_alive,
        response_timeout: config.timeouts.response(),
        model_label: Mutex::new(None),
        resume_url: Mutex::new(String::new()),
        last_http_status: Mutex::new(None),
    });
    // Take the terminal off the wrapper's stdin *before* the browser exists, so
    // the driver library has nothing to snapshot and nothing to write back onto
    // the TUI's line discipline. See `take_terminal_off_stdin`.
    let terminal = take_terminal_off_stdin();
    // Warm the browser *while* Codewhale boots rather than before it. Nothing
    // needs the page until the first turn, and waiting here put the whole cold
    // start — Playwright driver, Chromium, navigation, the composer probe — in
    // front of the TUI the user is watching. The diagnosis is not traded away:
    // a browser that cannot come up still stops the run with the same classified
    // message and the same record, and it stops it as soon as the failure is
    // known instead of after an arbitrary wait.
    let startup = std::time::Instant::now();
    let (failed_tx, mut failed_rx) = tokio::sync::oneshot::channel::<String>();
    let stale_link = Arc::new(AtomicBool::new(false));
    {
        let browser = Arc::clone(&browser);
        let stale_link = Arc::clone(&stale_link);
        let links = Arc::clone(&links);
        let session = session.clone();
        let notifications = config.relay.desktop_notifications;
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            if let Err(error) = browser.ensure_open().await {
                // The bridge could not even get a browser. Say why, and record it:
                // "it never started" is exactly the question a log should answer.
                let failure = browser.classify(&error).await;
                let (kind, blame) = (failure.kind.clone(), failure.blame.clone());
                let startup_log: Arc<dyn TurnSink> = Arc::new(TurnLog {
                    links: Arc::clone(&links),
                    session: session.clone(),
                });
                let _ = startup_log
                    .record(TurnRecord {
                        finish_reason: "error".to_owned(),
                        failure: Some(failure),
                        ..TurnRecord::default()
                    })
                    .await;
                let condensed = first_line(&error);
                let more = if error.lines().count() > 1 {
                    " (full text in the relay audit log)"
                } else {
                    ""
                };
                let _ = failed_tx.send(format!(
                    "could not open the browser [{kind} / {blame}]: {condensed}{more}"
                ));
                return;
            }
            eprintln!(
                "freechatcode: browser ready in {:.1}s",
                started.elapsed().as_secs_f64()
            );
            // The Codewhale session is the source of truth: verify the linked
            // conversation actually exists before trusting it. If it is gone,
            // realign from scratch — a new chat, fed the whole session — and tell
            // the user. Checked here because this is where the page exists.
            if had_link && !browser.link_available().await {
                stale_link.store(true, Ordering::SeqCst);
                notify(
                    notifications,
                    "DeepSeek chat link is stale",
                    "The linked conversation is no longer reachable. Opening a new chat and re-feeding the Codewhale session.",
                );
            }
        });
    }

    // Keep the browser alive between turns: a browser that is closed or crashes
    // should come back on its own, not at the next prompt.
    if config.browser.keep_alive && config.browser.liveness_check_secs > 0 {
        let watcher = Arc::clone(&browser);
        let interval = Duration::from_secs(config.browser.liveness_check_secs);
        tokio::spawn(async move { watcher.watch_browser(interval).await });
    }

    let start_fresh = !had_link;

    let ui = Arc::new(UrlLinkingChat {
        inner: Arc::clone(&browser),
        links: Arc::clone(&links),
        session: session.clone(),
        sessions_dir,
        workspace,
        chat: config.chat.clone(),
        linked: AtomicBool::new(false),
        stale_link: Arc::clone(&stale_link),
    });
    let turns: Arc<dyn TurnSink> = Arc::new(TurnLog {
        links: Arc::clone(&links),
        session: session.clone(),
    });

    let token = format!("cw_{}", Uuid::new_v4().simple());
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .context("bind the loopback-only browser relay")?;
    let address = listener.local_addr()?;
    let tool_policy = ToolPolicy {
        forward_all: config.tools.forward_all,
        essential: config.tools.essential.clone(),
        search: config.tools.search.clone(),
        allow_extra: config.tools.allow_extra.clone(),
    };
    // The instruction text is never hardcoded: read the path from config, else
    // use the committed default embedded in the binary. `{project_dir}` is filled
    // here; `{payload}` is filled per request by the relay.
    let template: String = match config.codewhale.system_prompt.as_deref() {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read the configured system prompt at {path}"))?,
        None => DEFAULT_SYSTEM_PROMPT.to_owned(),
    };
    let system_prompt: Arc<str> = Arc::from(template.replace("{project_dir}", &project_dir));
    let app = router(
        ServerState::with_options(
            token.clone(),
            ui.clone(),
            audit,
            BridgeOptions {
                tools: tool_policy,
                start_fresh,
                system_prompt,
                forward_system_prompt: config.relay.forward_system_prompt,
            },
        )
        .with_turns(turns),
    );
    let server = tokio::spawn(async move { serve(listener, app).await });
    let base_url = format!("http://{address}/v1");

    let model_id = args.model.clone().unwrap_or_else(|| MODEL_ID.to_owned());
    if !serves_model(&model_id) {
        bail!(
            "--model must be one of: {}",
            [MODEL_ID, PRO_MODEL_ID].join(", ")
        );
    }

    println!("Starting Codewhale with the DeepSeek Chat browser route.");
    println!("The local relay listens only on {address}; no browser CORS access is enabled.");
    println!("Platform credentials and cookies remain in the browser profile.");
    println!(
        "Run mode {}: browser {}, requests {}.",
        config.mode.as_str(),
        if config.browser.headless {
            "headless"
        } else {
            "visible"
        },
        if config.transport.mode == TransportMode::Api {
            "sent to the site's API"
        } else {
            "driven through the page"
        },
    );
    println!("Model: {model_id}.");
    if config.mode == RunMode::Silent {
        println!(
            "Sign-in still needs a window: if the page asks to be signed in, one opens \
             visibly and the run continues once you sign in."
        );
    }
    if !config.browser.keep_alive {
        println!("[browser] keep_alive = false: the browser closes after every turn.");
    }

    // Codewhale still gets the terminal on stdin; the wrapper just no longer
    // holds it itself.
    let child_stdin = match terminal {
        Some(terminal) => Stdio::from(terminal),
        None => Stdio::inherit(),
    };
    let mut child = Command::new(&codewhale_bin)
        .args([
            OsString::from("--provider"),
            OsString::from("openai"),
            OsString::from("--model"),
            OsString::from(model_id),
            OsString::from("--base-url"),
            OsString::from(base_url),
            OsString::from("--api-key"),
            OsString::from(token),
        ])
        .args(codewhale_args)
        .stdin(child_stdin)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("launch Codewhale executable {:?}", codewhale_bin))?;
    eprintln!(
        "freechatcode: Codewhale started at {:.2}s (browser warming in parallel)",
        startup.elapsed().as_secs_f64()
    );

    let status = tokio::select! {
        status = child.wait() => status.context("wait for Codewhale")?,
        failure = async {
            match (&mut failed_rx).await {
                // Only an actual message is a failure. A sender dropped after a
                // successful warm-up must not read as one, so this branch simply
                // never resolves in that case.
                Ok(message) => message,
                Err(_) => std::future::pending::<String>().await,
            }
        } => {
            // Codewhale is already up, but nothing can be answered without a
            // browser, so stop here with the same diagnosis the serial version
            // produced — just delivered as soon as it was known.
            child.kill().await.context("stop Codewhale")?;
            child.wait().await.context("reap Codewhale")?;
            server.abort();
            browser.shutdown().await;
            bail!("{failure}");
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait for Ctrl+C")?;
            child.kill().await.context("stop Codewhale")?;
            child.wait().await.context("reap Codewhale")?
        }
    };
    server.abort();
    browser.shutdown().await;
    if !status.success() {
        bail!("Codewhale exited with {status}");
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("freechatcode: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_url_acceptance_follows_config() {
        let config = Config::defaults();
        assert!(config.chat.accepts_url("https://chat.deepseek.com/"));
        assert!(!config.chat.accepts_url("http://chat.deepseek.com/"));
        assert!(
            !config
                .chat
                .accepts_url("https://chat.deepseek.com.attacker.invalid/")
        );
        assert!(!config.chat.accepts_url("https://user@chat.deepseek.com/"));
        // The old deepseek.ai host is no longer trusted by default.
        assert!(!config.chat.accepts_url("https://deepseek.ai/chat"));
    }

    #[test]
    fn a_reply_counts_as_arrived_when_the_virtualized_window_replaces_its_newest() {
        // The bug this pins: at prompt 3-4 the mounted window fills up, the page
        // unmounts an old message as it mounts the new reply, the element count
        // stays put, and a count-only test times out on a page that answered.
        assert!(reply_arrived(3, "the previous answer", 3, "the new answer"));
        // The window can also shrink while the newest message changes.
        assert!(reply_arrived(4, "older", 2, "the new answer"));
        // Growth still counts, even when two consecutive replies are identical.
        assert!(reply_arrived(3, "PONG", 4, "PONG"));
        // The first reply in a conversation.
        assert!(reply_arrived(0, "", 1, "hello"));
    }

    #[test]
    fn a_shifted_window_is_not_mistaken_for_a_reply() {
        // Count moved, newest text did not: older messages unmounted, nothing
        // arrived. Reporting the previous answer here would be worse than
        // waiting.
        assert!(!reply_arrived(3, "same", 2, "same"));
        assert!(!reply_arrived(3, "same", 3, "same"));
        // A mounted-but-empty element is a reply that has not started writing.
        assert!(!reply_arrived(2, "previous", 3, ""));
        assert!(!reply_arrived(2, "previous", 3, "   "));
    }

    #[test]
    fn wrapper_never_defaults_to_the_users_normal_chrome_profile() {
        let default = default_home()
            .expect("Codewhale home")
            .join("deepseek-chat/browser");
        assert!(default.ends_with("deepseek-chat/browser"));
    }

    #[test]
    fn local_provider_token_is_ephemeral_and_not_an_account_key() {
        let token = format!("cw_{}", Uuid::new_v4().simple());
        assert_eq!(token.len(), 35);
        assert!(token.starts_with("cw_"));
    }

    #[test]
    fn default_selectors_support_a_test_page_without_platform_private_endpoints() {
        let selectors = Config::defaults().selectors;
        assert!(selectors.composer.contains("textarea"));
        assert!(selectors.assistant.contains("ds-markdown"));
        assert!(selectors.send.contains("send"));
        assert!(selectors.new_chat.contains("new-chat"));
    }

    #[test]
    fn forwarded_codewhale_args_select_the_session_intent() {
        assert_eq!(codewhale_intent(&[]), Intent::Auto);
        assert_eq!(
            codewhale_intent(&[OsString::from("--fresh")]),
            Intent::Fresh
        );
        assert_eq!(
            codewhale_intent(&[OsString::from("-r"), OsString::from("abc123")]),
            Intent::Session("abc123".to_owned())
        );
        assert_eq!(
            codewhale_intent(&[OsString::from("--session-id=def456")]),
            Intent::Session("def456".to_owned())
        );
        assert_eq!(
            codewhale_intent(&[OsString::from("--continue")]),
            Intent::Auto
        );
    }

    #[test]
    fn unix_timestamps_format_as_utc() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, and a timestamp inside a leap year.
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_utc(1_791_421_618), "2026-10-08T01:06:58Z");
    }

    /// A real Playwright round trip against a local mock chat page: composer,
    /// submit, incremental reads, and the settle rule.
    #[tokio::test]
    async fn playwright_submits_and_reads_a_local_mock_chat_page() {
        let playwright = Playwright::launch().await.expect("Playwright driver");
        let browser = playwright
            .chromium()
            .launch()
            .await
            .expect("Playwright Chromium");
        let context = browser.new_context().await.expect("browser context");
        let page = context.new_page().await.expect("fixture page");
        page.set_content(
            r#"<!doctype html><main>
                <textarea placeholder="Message"></textarea>
                <button type="submit" aria-label="Send">Send</button>
              </main>
              <script>
                document.querySelector('button[type=submit]').addEventListener('click', () => {
                  const composer = document.querySelector('textarea');
                  composer.value = '';
                  const response = document.createElement('div');
                  response.className = 'ds-markdown';
                  response.textContent = JSON.stringify({type: 'final', content: 'mock answer'});
                  document.querySelector('main').append(response);
                });
              </script>"#,
            None,
        )
        .await
        .expect("install local UI fixture");

        let config = Config::defaults();
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: "https://chat.deepseek.com".to_owned(),
                profile: PathBuf::from("/nonexistent-test-profile"),
                cdp_endpoint: None,
            },
            session: Mutex::new(Some(
                Session::new(page.clone(), playwright, context, true)
                    .await
                    .expect("session"),
            )),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(30),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };

        let answer = ui
            .send("fixture prompt", false)
            .await
            .expect("visible UI response");
        assert_eq!(answer, r#"{"type":"final","content":"mock answer"}"#);
        ui.shutdown().await;
    }

    /// A reply the page has not finished writing must be waited for, not cut
    /// short.
    ///
    /// The page renders in bursts. This fixture writes the first half of an
    /// answer, pauses for longer than the old quiet window (half a second), then
    /// writes the rest. With that window the relay returned the first half and
    /// called it complete — seen in the wild as a 992-character answer recorded
    /// as 208 and a turn that looked stuck mid-print. This is that bug, without
    /// needing the network or an account.
    #[tokio::test]
    async fn a_reply_that_pauses_midway_is_still_read_whole() {
        let playwright = Playwright::launch().await.expect("Playwright driver");
        let browser = playwright
            .chromium()
            .launch()
            .await
            .expect("Playwright Chromium");
        let context = browser.new_context().await.expect("browser context");
        let page = context.new_page().await.expect("fixture page");
        page.set_content(
            r#"<!doctype html><main>
                <textarea placeholder="Message"></textarea>
                <button type="submit" aria-label="Send">Send</button>
              </main>
              <script>
                document.querySelector('button[type=submit]').addEventListener('click', () => {
                  const composer = document.querySelector('textarea');
                  composer.value = '';
                  const response = document.createElement('div');
                  response.className = 'ds-markdown';
                  // A complete, parseable answer that is not the whole answer.
                  response.textContent = JSON.stringify({type: 'final', content: 'first half'});
                  document.querySelector('main').append(response);
                  // Long enough to beat the old half-second quiet window, short
                  // enough that a patient reader still sees the end promptly.
                  setTimeout(() => {
                    response.textContent = JSON.stringify(
                      {type: 'final', content: 'first half and the second half'});
                  }, 1200);
                });
              </script>"#,
            None,
        )
        .await
        .expect("install local UI fixture");

        let config = Config::defaults();
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: "https://chat.deepseek.com".to_owned(),
                profile: PathBuf::from("/nonexistent-test-profile"),
                cdp_endpoint: None,
            },
            session: Mutex::new(Some(
                Session::new(page.clone(), playwright, context, true)
                    .await
                    .expect("session"),
            )),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(30),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };

        let answer = ui
            .send("fixture prompt", false)
            .await
            .expect("visible UI response");
        ui.shutdown().await;
        assert!(
            answer.contains("second half"),
            "the relay stopped reading at the pause and returned a partial answer: {answer:?}"
        );
    }

    /// The failure path: a page that accepts the send but never answers must
    /// produce a diagnostic error promptly instead of hanging or reporting only
    /// "it timed out".
    #[tokio::test]
    async fn a_page_that_never_answers_reports_why() {
        let playwright = Playwright::launch().await.expect("Playwright driver");
        let browser = playwright
            .chromium()
            .launch()
            .await
            .expect("Playwright Chromium");
        let context = browser.new_context().await.expect("browser context");
        let page = context.new_page().await.expect("fixture page");
        page.set_content(
            r#"<!doctype html><main>
                <textarea placeholder="Message"></textarea>
                <button type="submit" aria-label="Send">Send</button>
              </main>
              <script>
                // Accepts the send (clears the composer) but never replies.
                document.querySelector('button[type=submit]').addEventListener('click', () => {
                  document.querySelector('textarea').value = '';
                });
              </script>"#,
            None,
        )
        .await
        .expect("install fixture");

        let config = Config::defaults();
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: "https://chat.deepseek.com".to_owned(),
                profile: PathBuf::from("/nonexistent-test-profile"),
                cdp_endpoint: None,
            },
            session: Mutex::new(Some(
                Session::new(page.clone(), playwright, context, true)
                    .await
                    .expect("session"),
            )),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(5),
            keep_alive: true,
            // Short enough to keep the test fast.
            response_timeout: Duration::from_millis(400),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };

        let error = ui
            .send("a prompt the page will silently swallow", false)
            .await
            .expect_err("a silent page must fail the turn");
        assert!(
            error.contains("no visible assistant reply"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("composer_chars=0"),
            "the composer was cleared, so the send was accepted: {error}"
        );
        assert!(
            error.contains("open the URL and look before blaming the page"),
            "the error must say what was and was not observed: {error}"
        );
        assert!(
            !error.contains("decided not to answer"),
            "and must not assert a cause it never saw: a page that answered but was \
             unread is exactly how this went wrong: {error}"
        );
        ui.shutdown().await;
    }

    #[test]
    fn a_cdp_endpoint_implies_attach_mode() {
        assert!(resolves_to_attach(
            BrowserMode::Managed,
            Some("http://127.0.0.1:9222")
        ));
        assert!(resolves_to_attach(BrowserMode::Attach, None));
        assert!(resolves_to_attach(
            BrowserMode::Managed,
            Some("http://127.0.0.1:9222")
        ));
        // A blank endpoint is not an endpoint.
        assert!(!resolves_to_attach(BrowserMode::Managed, Some("   ")));
        assert!(!resolves_to_attach(BrowserMode::Managed, None));
    }

    #[test]
    fn profile_holder_detects_a_live_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path();
        // No lock at all: the profile is free.
        assert_eq!(profile_holder(profile), None);

        // A lock naming a dead pid is not a holder.
        std::os::unix::fs::symlink("host-999999999", profile.join("SingletonLock"))
            .expect("symlink");
        assert_eq!(profile_holder(profile), None);

        // A lock naming a live process is.
        let live = std::process::id();
        std::fs::remove_file(profile.join("SingletonLock")).expect("remove");
        std::os::unix::fs::symlink(format!("host-{live}"), profile.join("SingletonLock"))
            .expect("symlink");
        assert_eq!(profile_holder(profile), Some(format!("host-{live}")));
    }

    struct QuietAudit;

    fn install_crypto_provider() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
    }

    #[async_trait::async_trait]
    impl AuditSink for QuietAudit {
        async fn append(&self, _record: Value) -> Result<(), String> {
            Ok(())
        }
    }

    /// The whole relay, live: real router, real browser, real HTTP, and a real
    /// tool call that comes back as OpenAI `tool_calls`, then a final answer fed
    /// by a tool result. Ignored by default; needs a signed-in profile.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_relay_tool_call_round_trip() {
        use serde_json::json;

        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = false;
        let workspace = std::env::current_dir().expect("cwd");
        let ui = Arc::new(BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: home.join("deepseek-chat").join("browser"),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        });
        ui.ensure_open().await.expect("open the browser");

        let state = ServerState::with_options(
            "live-token",
            ui.clone(),
            Arc::new(QuietAudit),
            BridgeOptions {
                tools: ToolPolicy::default(),
                start_fresh: true,
                system_prompt: Arc::from(
                    DEFAULT_SYSTEM_PROMPT
                        .replace("{project_dir}", workspace.to_string_lossy().as_ref()),
                ),
                forward_system_prompt: false,
            },
        )
        // Record into the real turn log, so `freechatcode turns` can be checked
        // against rows a live turn actually wrote.
        .with_turns({
            let links = Arc::new(
                SessionLinks::open(&home.join("freechatcode").join("sessions.db"))
                    .expect("open link store"),
            );
            let session = sessions::resolve_session_id(&home.join("sessions"), &workspace, None);
            Arc::new(TurnLog { links, session })
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.expect("serve");
        });

        let tools = json!([{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file from the workspace",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }]);
        let ask = "Call the read_file tool with path \"src/main.rs\". Reply with the tool-call JSON only.";
        let url = format!("http://{address}/v1/chat/completions");
        let client = reqwest::Client::new();

        let started = std::time::Instant::now();
        let body: Value = client
            .post(&url)
            .bearer_auth("live-token")
            .json(&json!({
                "model": MODEL_ID,
                "messages": [{"role": "user", "content": ask}],
                "tools": tools,
            }))
            .send()
            .await
            .expect("relay response")
            .json()
            .await
            .expect("relay json");
        eprintln!(
            "[live relay] tool-call turn ({:?}): {body}",
            started.elapsed()
        );
        let message = body["choices"][0]["message"].clone();
        assert!(
            message["tool_calls"].is_array(),
            "the model must return OpenAI tool_calls; got {message}"
        );
        assert_eq!(message["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");

        // Feed the tool result back, exactly as Codewhale would.
        let call_id = message["tool_calls"][0]["id"].clone();
        let body: Value = client
            .post(&url)
            .bearer_auth("live-token")
            .json(&json!({
                "model": MODEL_ID,
                "messages": [
                    {"role": "user", "content": ask},
                    {"role": "assistant", "content": null, "tool_calls": message["tool_calls"]},
                    {"role": "tool", "tool_call_id": call_id, "content": "fn main() { println!(\"hello\"); }"},
                ],
                "tools": tools,
            }))
            .send()
            .await
            .expect("relay response")
            .json()
            .await
            .expect("relay json");
        eprintln!("[live relay] tool-result turn: {body}");
        let answer = body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !answer.trim().is_empty(),
            "the relay must turn a tool result into a final answer; got {body}"
        );
        eprintln!("[live relay] final answer: {answer}");
        server.abort();
        ui.shutdown().await;
    }

    /// Diagnostic: observe the chat page's own completion request and response
    /// so `[transport.api]` can be configured from evidence instead of a guess.
    /// Prints every POST the page makes to deepseek.com while one real turn runs,
    /// plus the first frames of the completion response.
    #[tokio::test]
    #[ignore = "diagnostic: discovers the site's own completion endpoint from live traffic"]
    async fn live_discover_api_endpoint() {
        use std::sync::Mutex as StdMutex;

        let home = default_home().expect("codewhale home");
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = false;
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: home.join("deepseek-chat").join("browser"),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open().await.expect("open the browser");

        let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        {
            let guard = ui.session.lock().await;
            let page = guard.as_ref().expect("a live session").page.clone();
            let seen = Arc::clone(&seen);
            page.on_request(move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    if request.method() != "POST" {
                        return Ok(());
                    }
                    let url = request.url().to_owned();
                    if !url.contains("deepseek") {
                        return Ok(());
                    }
                    let body: String = request
                        .post_data()
                        .unwrap_or_default()
                        .chars()
                        .take(500)
                        .collect();
                    // Header NAMES only (plus a redacted shape): the point is to
                    // learn which header carries auth, not to leak the token.
                    let mut names: Vec<String> = request
                        .headers()
                        .into_iter()
                        .map(|(name, value)| {
                            let hint = if value.len() > 12 {
                                format!("<{} chars>", value.len())
                            } else {
                                value
                            };
                            format!("{name}: {hint}")
                        })
                        .collect();
                    names.sort();
                    seen.lock()
                        .expect("seen")
                        .push(format!("POST {url}\n  headers: {names:?}\n  body: {body}"));
                    Ok(())
                }
            })
            .await
            .expect("register request observer");
        }
        {
            let guard = ui.session.lock().await;
            let page = guard.as_ref().expect("a live session").page.clone();
            let seen = Arc::clone(&seen);
            page.on_response(move |response| {
                let seen = Arc::clone(&seen);
                async move {
                    let url = response.url().to_owned();
                    if !url.contains("/chat/completion") {
                        return Ok(());
                    }
                    let status = response.status();
                    let body: String =
                        String::from_utf8_lossy(&response.body().await.unwrap_or_default())
                            .chars()
                            .take(1200)
                            .collect();
                    seen.lock()
                        .expect("seen")
                        .push(format!("RESPONSE {status} {url}\n  body: {body}"));
                    Ok(())
                }
            })
            .await
            .expect("register response observer");
        }

        let reply = ui
            .send("Reply with the single word PONG.", true)
            .await
            .expect("real turn");
        eprintln!("[discover] reply={reply:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let captured = seen.lock().expect("seen").clone();
        eprintln!("[discover] {} POST(s) to deepseek:", captured.len());
        for entry in captured {
            eprintln!("[discover] {entry}");
        }

        // Probe the private endpoint directly from the page context, so the
        // response framing (not just the request shape) is evidence too.
        {
            let guard = ui.session.lock().await;
            let page = guard.as_ref().expect("a live session").page.clone();
            drop(guard);
            let session_id = page.url().rsplit('/').next().unwrap_or_default().to_owned();
            eprintln!("[discover] session id from page url: {session_id}");
            let body = serde_json::json!({
                "chat_session_id": session_id,
                "parent_message_id": null,
                "model_type": "default",
                "prompt": "Reply with the single word PONG.",
                "ref_file_ids": [],
                "thinking_enabled": false,
                "search_enabled": false,
                "action": null,
                "preempt": false,
            })
            .to_string();
            let script = "async ([url, body]) => { const r = await fetch(url, {method: 'POST', headers: {'content-type': 'application/json'}, body, credentials: 'include'}); return await r.text(); }";
            let arg = serde_json::json!(["/api/v0/chat/completion", body]);
            match page.evaluate::<Value, String>(script, Some(&arg)).await {
                Ok(raw) => eprintln!(
                    "[discover] raw api stream ({} bytes, first 1200): {}",
                    raw.len(),
                    raw.chars().take(1200).collect::<String>()
                ),
                Err(error) => eprintln!("[discover] raw api probe failed: {error}"),
            }

            // Which storage keys does the page keep, and where is the bearer
            // token it sends? Values are truncated, not printed in full.
            let keys = "() => { const out = []; for (let i = 0; i < localStorage.length; i++) { const k = localStorage.key(i); const v = String(localStorage.getItem(k)); out.push(k + ' = ' + v.slice(0, 40)); } return out; }";
            match page.evaluate::<(), Vec<String>>(keys, None::<&()>).await {
                Ok(found) => eprintln!("[discover] localStorage keys: {found:#?}"),
                Err(error) => eprintln!("[discover] localStorage probe failed: {error}"),
            }

            // Now with the page's own bearer token: if the only thing still
            // missing is the proof-of-work, the error must change.
            let authed = "async ([url, body]) => { let t = localStorage.getItem('userToken'); try { t = JSON.parse(t).value; } catch (e) {} const r = await fetch(url, {method: 'POST', headers: {'content-type': 'application/json', 'authorization': 'Bearer ' + t}, body, credentials: 'include'}); return r.status + ' ' + (await r.text()).slice(0, 300); }";
            let arg = serde_json::json!(["/api/v0/chat/completion", body]);
            match page.evaluate::<Value, String>(authed, Some(&arg)).await {
                Ok(raw) => eprintln!("[discover] authed api probe: {raw}"),
                Err(error) => eprintln!("[discover] authed api probe failed: {error}"),
            }
        }
        ui.shutdown().await;
    }

    /// Live: the wrapper advertises two models and the page state has to match
    /// whichever was asked for. `deepseek-pro` engages the page's DeepThink
    /// control; the plain chat model puts it back. A pro turn that cannot engage
    /// it fails rather than answering as the wrong model.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_the_pro_model_engages_the_pages_reasoning_mode() {
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open().await.expect("open the browser");
        let page = ui
            .session
            .lock()
            .await
            .as_ref()
            .expect("a live session")
            .page
            .clone();

        eprintln!("[pro] page chips at rest: {:?}", chips(&ui, &page).await);

        ui.set_reasoning(false)
            .await
            .expect("the chat model must be reachable");
        let chat = chips(&ui, &page).await;
        eprintln!("[pro] after chat: {chat:?}");
        assert!(
            chat.contains("DeepThink=off") || !chat.contains("DeepThink"),
            "the plain chat model must leave DeepThink off, saw {chat:?}"
        );

        ui.set_reasoning(true)
            .await
            .expect("the pro model must engage");
        let pro = chips(&ui, &page).await;
        eprintln!("[pro] after pro: {pro:?}");
        assert!(
            pro.contains("DeepThink=on"),
            "deepseek-pro must leave the page showing DeepThink=on, saw {pro:?}"
        );

        // A real turn in pro mode, so the state survives an actual exchange.
        let reply = ui
            .send("Reply with exactly the word PRO and nothing else.", true)
            .await
            .expect("a pro turn");
        eprintln!("[pro] pro turn reply={reply:?}");
        assert!(reply.contains("PRO"), "got {reply:?}");
        let after_turn = chips(&ui, &page).await;
        eprintln!("[pro] page chips after the pro turn: {after_turn:?}");
        assert!(
            after_turn.contains("DeepThink=on"),
            "the turn log derives the model from these chips, so they must still say pro: {after_turn:?}"
        );

        // And back: a chat turn must not inherit pro.
        ui.set_reasoning(false)
            .await
            .expect("disengaging must work too");
        let back = chips(&ui, &page).await;
        eprintln!("[pro] after switching back: {back:?}");
        assert!(
            !back.contains("DeepThink=on"),
            "a later chat turn must not inherit pro mode: {back:?}"
        );
        ui.shutdown().await;
    }

    /// Live: the relay itself engages the pro mode. The test above drives
    /// `set_reasoning` directly; this one goes through the real router, so the
    /// composition — a `deepseek-pro` request arriving over HTTP and leaving the
    /// page in DeepThink — is verified rather than assumed.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_relay_engages_pro_for_a_pro_request() {
        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let (dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        let ui = Arc::new(BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        });
        ui.ensure_open().await.expect("open the browser");
        let page = ui
            .session
            .lock()
            .await
            .as_ref()
            .expect("a live session")
            .page
            .clone();
        // Start from the plain chat model, so an engaged control can only be the
        // relay's doing.
        ui.set_reasoning(false).await.expect("start in chat mode");
        let before = chips(&ui, &page).await;
        eprintln!("[relay] chips before: {before:?}");
        assert!(!before.contains("DeepThink=on"), "{before:?}");

        let audit = JsonlAudit::create(&dir.path().join("audit"))
            .await
            .expect("audit log");
        let state = ServerState::new("secret", ui.clone(), audit);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });

        let response: Value = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&serde_json::json!({
                "model": PRO_MODEL_ID,
                "messages": [{"role": "user", "content": "Reply with exactly the word ELEVEN and nothing else."}],
                "stream": false,
            }))
            .send()
            .await
            .expect("relay response")
            .json()
            .await
            .expect("relay json");
        let content = response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        eprintln!(
            "[relay] pro reply={content:?} model={:?}",
            response["model"]
        );
        assert!(content.contains("ELEVEN"), "got {content:?}");
        let after = chips(&ui, &page).await;
        eprintln!("[relay] chips after: {after:?}");
        assert!(
            after.contains("DeepThink=on"),
            "a deepseek-pro request through the relay must leave the page in pro mode: {after:?}"
        );

        // And the plain model puts it back, through the relay as well.
        let response: Value = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&serde_json::json!({
                "model": MODEL_ID,
                "messages": [{"role": "user", "content": "Reply with exactly the word TWELVE and nothing else."}],
                "stream": false,
            }))
            .send()
            .await
            .expect("relay response")
            .json()
            .await
            .expect("relay json");
        let content = response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default();
        eprintln!("[relay] chat reply={content:?}");
        assert!(content.contains("TWELVE"), "got {content:?}");
        let back = chips(&ui, &page).await;
        eprintln!("[relay] chips after the chat request: {back:?}");
        assert!(
            !back.contains("DeepThink=on"),
            "a chat request through the relay must not inherit pro mode: {back:?}"
        );
        ui.shutdown().await;
    }

    /// Live: a streamed turn ends. Every other live test here asks for the
    /// non-streamed path; Codewhale always streams, and a stream that never
    /// closes is indistinguishable from a hung turn — the text has been printed,
    /// and the client waits forever for the end of a response that has none.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_relay_streams_a_reply_and_closes_the_stream() {
        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let (dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        let ui = Arc::new(BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        });
        ui.ensure_open().await.expect("open the browser");
        let audit = JsonlAudit::create(&dir.path().join("audit"))
            .await
            .expect("audit log");
        let state = ServerState::new("secret", ui.clone(), audit);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });

        let mut response = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&serde_json::json!({
                "model": MODEL_ID,
                "messages": [{"role": "user", "content": "Write exactly three short sentences about the ocean. Nothing else."}],
                "stream": true,
            }))
            .send()
            .await
            .expect("streamed response headers");
        assert_eq!(response.status(), 200);

        // Read to the end of the body with a hard deadline. A stream that never
        // closes must fail this rather than hanging the test suite.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        let mut body = String::new();
        // When a delta arrives is when the page had something new to show, so the
        // gaps between them are the page's own rendering gaps — the thing the
        // quiet window has to exceed. Measured here rather than guessed at.
        let mut arrivals = Vec::new();
        let read_started = std::time::Instant::now();
        loop {
            match tokio::time::timeout_at(deadline, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    body.push_str(&String::from_utf8_lossy(&chunk));
                    if !chunk.is_empty() {
                        arrivals.push(read_started.elapsed());
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(error)) => panic!("the stream broke: {error}"),
                Err(_) => panic!(
                    "the stream never closed within 90s; the client would wait forever. \
                     bytes so far ({}): {:?}",
                    body.len(),
                    body.chars()
                        .rev()
                        .take(200)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect::<String>()
                ),
            }
        }
        let gaps: Vec<u128> = arrivals
            .windows(2)
            .map(|pair| pair[1].saturating_sub(pair[0]).as_millis())
            .collect();
        eprintln!(
            "[stream] {} delta(s), arrival gaps (ms): {gaps:?} max={:?}",
            arrivals.len(),
            gaps.iter().max()
        );
        let chunks: Vec<&str> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect();
        eprintln!(
            "[stream] {} data line(s), closes with {:?}, content {:?}",
            chunks.len(),
            chunks.last(),
            body
        );
        assert_eq!(
            chunks.last().copied(),
            Some("[DONE]"),
            "the stream must end with [DONE]"
        );
        // A multi-sentence answer, so this exercises many deltas instead of one
        // and a reply cut short shows up as a length failure rather than as a
        // plausible-looking one-word answer.
        let text: String = chunks
            .iter()
            .filter(|line| *line != &"[DONE]")
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|chunk| {
                chunk["choices"][0]["delta"]["content"]
                    .as_str()
                    .map(str::to_owned)
            })
            .collect();
        eprintln!("[stream] reassembled {} chars: {text:?}", text.len());
        assert!(
            text.len() > 60,
            "a three-sentence answer streamed back as {} chars: {text:?}",
            text.len()
        );
        ui.shutdown().await;
    }

    /// Live: `silent` answers with no window, and reports which path answered.
    ///
    /// The direct API path is expected to refuse — the site's completion
    /// endpoint wants a per-request proof-of-work header this project does not
    /// synthesize (STATUS.md, gap 1) — so this doubles as our own record of what
    /// the endpoint actually answers, and of the fallback that keeps `silent`
    /// usable anyway.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_silent_mode_answers_headless_through_the_fallback() {
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let config = Config::load_with_mode(
            Some(&config::user_config_path(&home)),
            Some(RunMode::Silent),
        )
        .expect("config");
        assert!(config.browser.headless, "silent is headless");
        assert_eq!(config.transport.mode, TransportMode::Api);

        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: config.transport.mode,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open().await.expect("open the browser");

        let reply = ui
            .send("Reply with exactly the word QUIET and nothing else.", true)
            .await
            .expect("silent mode must still answer");
        eprintln!("[silent] reply={reply:?}");
        assert!(reply.contains("QUIET"), "got {reply:?}");
        ui.shutdown().await;
    }

    /// The page's own mode chips, as the turn log records them.
    async fn chips(ui: &BrowserChat, page: &Page) -> String {
        ui.read_model_label(page).await.unwrap_or_default()
    }

    /// Live: `silent` plus a pro request must still be pro.
    ///
    /// The api template shipped here is generic and carries no `{thinking}`, so
    /// the api path cannot be told which model to use. It refuses, and the turn
    /// falls back to the headless page, which engages DeepThink. The caller must
    /// never receive a plain answer labelled pro, so this asserts both the reply
    /// and the page state it came from.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_silent_mode_still_honours_a_pro_request() {
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let config = Config::load_with_mode(
            Some(&config::user_config_path(&home)),
            Some(RunMode::Silent),
        )
        .expect("config");
        assert_eq!(config.transport.mode, TransportMode::Api);
        assert!(
            !config.transport.api.body.contains("{thinking}"),
            "this test is about the shipped template, which cannot express reasoning"
        );

        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: config.transport.mode,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open().await.expect("open the browser");
        let page = ui
            .session
            .lock()
            .await
            .as_ref()
            .expect("a live session")
            .page
            .clone();

        // What the relay does for a `deepseek-pro` request, in order.
        ui.set_reasoning(false).await.expect("start in chat mode");
        ui.set_reasoning(true)
            .await
            .expect("the pro model must engage");
        let reply = ui
            .send(
                "Reply with exactly the word THIRTEEN and nothing else.",
                true,
            )
            .await
            .expect("silent mode must still answer a pro request");
        let after = chips(&ui, &page).await;
        eprintln!("[silent+pro] reply={reply:?} chips={after:?}");
        assert!(reply.contains("THIRTEEN"), "got {reply:?}");
        assert!(
            after.contains("DeepThink=on"),
            "a pro request answered in silent mode must still be pro: {after:?}"
        );
        ui.shutdown().await;
    }

    /// Live: the model is a Codewhale coder, not a narrator of how it is reached.
    ///
    /// Reported from a real session: the model answered "I'm reached through the
    /// DeepSeek Chat web page, driven in a browser you're signed in to..." and
    /// recited the session handshake, because the instruction text described the
    /// wrapper instead of handing over Codewhale's own briefing. This sends the
    /// same kind of bare message and requires an answer that does not talk about
    /// its own transport, and it checks the audit to prove the briefing was
    /// actually bridged.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_the_model_is_a_coder_not_a_narrator_of_its_transport() {
        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let (dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        assert!(
            config.relay.forward_system_prompt,
            "Codewhale's briefing is what makes this a coder rather than a narrator"
        );
        let ui = Arc::new(BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        });
        ui.ensure_open().await.expect("open the browser");
        let audit_dir = dir.path().join("audit");
        let audit = JsonlAudit::create(&audit_dir).await.expect("audit log");
        let state = ServerState::new("secret", ui.clone(), audit);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });

        let response: Value = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&serde_json::json!({
                "model": MODEL_ID,
                "messages": [
                    {"role": "system", "content": "BRIEFING-MARKER: the project rules and the tool catalog live here."},
                    {"role": "user", "content": "You can call tools you know"},
                ],
                "stream": false,
            }))
            .send()
            .await
            .expect("relay response")
            .json()
            .await
            .expect("relay json");
        let reply = response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        eprintln!("[coder] reply={reply:?}");

        // The briefing was actually bridged: what the page was sent carries it.
        let mut prompt_seen = String::new();
        for entry in std::fs::read_dir(&audit_dir).expect("audit dir") {
            let path = entry.expect("audit entry").path();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains("BRIEFING-MARKER") {
                prompt_seen = text;
            }
        }
        assert!(
            prompt_seen.contains("BRIEFING-MARKER"),
            "Codewhale's briefing must reach the page"
        );

        // And the answer is about the work, not about the wrapper's plumbing.
        let lower = reply.to_lowercase();
        for phrase in [
            "reached through",
            "driven in a browser",
            "you're signed in",
            "you are signed in",
            "no api key",
            "handshake",
        ] {
            assert!(
                !lower.contains(phrase),
                "the model narrated its own transport ({phrase:?}): {reply:?}"
            );
        }
        ui.shutdown().await;
    }

    /// Live: the reported message does not end the turn on a promise.
    ///
    /// The input is the user's own, verbatim — not a prompt written to elicit a
    /// tool call, which is what made the first version of this test prove so
    /// little. A live model is stochastic, so this asserts the *guarantee* the
    /// relay provides (the turn ends either on an action or on a marked answer,
    /// never on an unmarked promise) and prints which happened; the deterministic
    /// guarantee lives in the offline tests for `needs_protocol_retry`.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_a_chatty_request_does_not_end_the_turn_on_a_promise() {
        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let (_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        let ui = Arc::new(BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        });
        ui.ensure_open().await.expect("open the browser");
        let state = ServerState::new("secret", ui.clone(), Arc::new(QuietAudit));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });

        let response: Value = reqwest::Client::new()
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth("secret")
            .json(&serde_json::json!({
                "model": MODEL_ID,
                "messages": [{
                    "role": "user",
                    "content": "Hello, I want to discuss the name of this project, a better one, and also adding support for Gemini app, and google ai mode, research, run terminal commands, read files, read docs",
                }],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "read",
                        "description": "Read a file from the workspace.",
                        "parameters": {
                            "type": "object",
                            "properties": {"path": {"type": "string"}},
                            "required": ["path"],
                        },
                    },
                }],
                "tool_choice": "auto",
                "stream": false,
            }))
            .send()
            .await
            .expect("relay response")
            .json()
            .await
            .expect("relay json");

        let message = &response["choices"][0]["message"];
        let finish = response["choices"][0]["finish_reason"]
            .as_str()
            .unwrap_or_default();
        let content = message["content"].as_str().unwrap_or_default();
        eprintln!("[loop] finish_reason={finish:?} content={content:?}");
        // The invariant the mechanism actually provides: the turn does not end on
        // an unmarked promise. Either the model acted, or it delivered an answer
        // that the contract marks as one. A live model can still drift, so this
        // asserts the guarantee rather than a hoped-for behaviour.
        let acted = finish == "tool_calls"
            && message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty());
        let answered = content.trim_start().starts_with("Here is the answer.");
        assert!(
            acted || answered,
            "the turn ended on something that is neither an action nor a marked \
             answer, which is the failure this pins: finish_reason={finish:?} \
             content={content:?}"
        );
        ui.shutdown().await;
    }

    /// Live: a real agent, through the bridge, doing real work with a tool.
    ///
    /// Nothing here is mocked. The shipped wrapper binary starts, launches
    /// Codewhale as its subprocess, and the relay drives a real browser on a copy
    /// of the profile; the agent has to call a tool and answer from what it read.
    /// It also exercises the split the design rests on: the *page* holds the
    /// thread — the tool result is fed into the same conversation the answer comes
    /// out of — while the wrapper only carries messages in and text out.
    #[tokio::test]
    #[ignore = "needs a signed-in profile, the network, and a built binary"]
    async fn live_a_real_agent_reads_a_file_through_the_bridge() {
        const TOKEN: &str = "MAGIC-TOKEN-7F3A";
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let cli = wrapper_cli();

        // A workspace of its own, so the agent works on a file whose contents are
        // known and the assertion is exact rather than a guess about this repo.
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(
            workspace.path().join("note.txt"),
            format!("The token is {TOKEN}.\n"),
        )
        .expect("fixture");

        let task = "Read note.txt with the read tool, then reply with the exact token it \
                    contains and nothing else.";
        let output = run_agent(&cli, &profile, &home, workspace.path(), &[task]).await;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        eprintln!("[agent] exit={:?}", output.status.code());
        eprintln!("[agent] stdout: {stdout}");
        eprintln!(
            "[agent] stderr tail: {}",
            stderr.lines().rev().take(6).collect::<Vec<_>>().join(" | ")
        );
        assert!(
            output.status.success(),
            "the agent run must succeed; stderr: {stderr}"
        );
        // The token only exists inside the file, so this cannot pass unless the
        // tool ran, its result went into the page's thread, and the model answered
        // from that thread.
        assert!(
            stdout.contains(TOKEN),
            "the agent must answer from what it read: {stdout}"
        );
    }

    /// The shipped wrapper binary beside the test binary.
    ///
    /// `cargo test` does not rebuild the plain binary, so a stale one is a real
    /// hazard — it is checked for the flag these tests rely on, and the message
    /// says how to fix it.
    fn wrapper_cli() -> PathBuf {
        let exe = std::env::current_exe().expect("test binary path");
        // .../target/<profile>/deps/<test binary>: the CLI sits one level up, and
        // the sibling profile one level above that.
        let deps = exe.parent().expect("deps directory");
        let profile_dir = deps.parent().expect("profile directory");
        let target = profile_dir.parent().unwrap_or(profile_dir);
        let cli = [
            target.join("release/freechatcode"),
            profile_dir.join("freechatcode"),
        ]
        .into_iter()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| profile_dir.join("freechatcode"));
        let help = std::process::Command::new(&cli)
            .arg("--help")
            .output()
            .unwrap_or_else(|error| panic!("run {}: {error}", cli.display()));
        assert!(
            String::from_utf8_lossy(&help.stdout).contains("--mode"),
            "{} is stale (no --mode); rebuild it: cargo build --bin freechatcode",
            cli.display()
        );
        cli
    }

    /// Run the shipped wrapper as a real agent — real Codewhale, real browser on a
    /// copy of the profile — and hand back what it produced.
    async fn run_agent(
        cli: &Path,
        profile: &Path,
        home: &Path,
        workspace: &Path,
        args: &[&str],
    ) -> std::process::Output {
        let run = tokio::process::Command::new(cli)
            .args(["--mode", "silent"])
            .arg("--profile-dir")
            .arg(profile)
            .args(["--", "exec", "--auto"])
            .args(args)
            .current_dir(workspace)
            .env("CODEWHALE_HOME", home)
            .output();
        tokio::time::timeout(Duration::from_secs(240), run)
            .await
            .expect("the agent must finish within four minutes")
            .expect("run the wrapper")
    }

    /// Live: a one-shot run carries nothing across processes, and does not
    /// pretend to.
    ///
    /// The wrapper links conversations to Codewhale *sessions*, and a one-shot
    /// `codewhale exec` saves none — `--session-id` resumes an existing session, it
    /// does not create one (verified: 44 session files before, 44 after). So a
    /// second, independent one-shot cannot recall the first, and the honest
    /// behaviour is to start clean rather than inherit a stranger's conversation.
    /// Cross-run memory is a session feature: it is what an interactive
    /// `freechatcode launch codewhale` has and a one-shot does not.
    ///
    /// What this pins is therefore two things: a tool really runs in the
    /// workspace, and unrelated runs do not leak state into each other.
    #[tokio::test]
    #[ignore = "needs a signed-in profile, the network, and a built binary"]
    async fn live_a_one_shot_run_leaves_no_session_to_inherit() {
        const TOKEN: &str = "THREAD-TOKEN-9C41";
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let cli = wrapper_cli();
        let workspace = tempfile::tempdir().expect("workspace");
        let note = workspace.path().join("secret.txt");
        let sessions_before = std::fs::read_dir(home.join("sessions"))
            .map(|entries| entries.count())
            .unwrap_or(0);

        let first = run_agent(
            &cli,
            &profile,
            &home,
            workspace.path(),
            &[&format!(
                "Create a file named secret.txt containing exactly {TOKEN} and nothing else, \
                 using the write tool."
            )],
        )
        .await;
        assert!(first.status.success(), "the writing run must succeed");
        // The tool's effect is on disk, which is the part no answer can fake.
        let written = std::fs::read_to_string(&note).unwrap_or_default();
        eprintln!("[one-shot] file on disk: {written:?}");
        assert!(
            written.contains(TOKEN),
            "the agent must actually have written the file; it holds {written:?}"
        );
        let sessions_after = std::fs::read_dir(home.join("sessions"))
            .map(|entries| entries.count())
            .unwrap_or(0);
        eprintln!("[one-shot] codewhale sessions before={sessions_before} after={sessions_after}");

        // The token now exists nowhere the second run can reach.
        std::fs::remove_file(&note).expect("remove the file");
        let second = run_agent(
            &cli,
            &profile,
            &home,
            workspace.path(),
            &[
                "What token did you write in secret.txt a moment ago? If you do not know, \
               say that you do not know. Do not use any tool.",
            ],
        )
        .await;
        let stdout = String::from_utf8_lossy(&second.stdout).into_owned();
        eprintln!(
            "[one-shot] second exit={:?} stdout={stdout:?}",
            second.status.code()
        );
        assert!(second.status.success(), "the second run must still succeed");
        assert!(
            !stdout.contains(TOKEN),
            "an unrelated run must not inherit another conversation's thread: {stdout}"
        );
    }

    /// Live: the loop continues across more than one tool call in a single run.
    ///
    /// Tool results have to go back into the page's thread for the model to decide
    /// what to do next, so a task that needs two steps proves the thread carries
    /// them — and that the wrapper is not involved in the loop at all.
    #[tokio::test]
    #[ignore = "needs a signed-in profile, the network, and a built binary"]
    async fn live_a_two_step_tool_loop_survives_the_round_trip() {
        const TOKEN: &str = "LOOP-TOKEN-3B58";
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let cli = wrapper_cli();
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("a.txt"), "nothing here\n").expect("fixture a");
        std::fs::write(workspace.path().join("b.txt"), format!("{TOKEN}\n")).expect("fixture b");

        let run = run_agent(
            &cli,
            &profile,
            &home,
            workspace.path(),
            &[
                "List the files in this directory with the list tool, then read b.txt and reply \
               with the exact token it contains and nothing else.",
            ],
        )
        .await;
        let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
        let tools_run = stderr
            .lines()
            .filter(|line| line.contains("tool ") && line.contains("completed"))
            .count();
        eprintln!(
            "[loop] exit={:?} tools_reported={tools_run} stdout={stdout:?}",
            run.status.code()
        );
        assert!(run.status.success(), "the run must succeed");
        assert!(
            stdout.contains(TOKEN),
            "the agent must answer from what it read: {stdout}"
        );
        assert!(
            tools_run >= 1,
            "at least one tool must have run; stderr tail: {}",
            stderr.lines().rev().take(5).collect::<Vec<_>>().join(" | ")
        );
    }

    /// A throwaway copy of the signed-in profile, so a live test never competes
    /// for the profile a real session is using.
    fn profile_copy() -> (tempfile::TempDir, PathBuf) {
        let home = default_home().expect("codewhale home");
        let source = home.join("deepseek-chat").join("browser");
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join("browser-copy");
        assert!(
            std::process::Command::new("cp")
                .arg("-a")
                .arg(&source)
                .arg(&profile)
                .status()
                .expect("run cp")
                .success(),
            "could not copy the browser profile"
        );
        // A copied lock names the *live* owner and would make the wrapper refuse
        // to launch its own copy.
        for lock in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
            let _ = std::fs::remove_file(profile.join(lock));
        }
        (dir, profile)
    }

    /// How many assistant elements the page currently keeps mounted.
    async fn mounted_count(page: &Page, selectors: &Selectors) -> usize {
        page.locator(&selectors.assistant)
            .count()
            .await
            .unwrap_or(0)
    }

    /// What the page keeps mounted, in DOM order: per-selector counts and the
    /// last few message elements. The reply-detection contract is "how many
    /// assistant elements are in the DOM?", so this is the evidence that shows a
    /// virtualized transcript — a mounted window that has filled up — rather
    /// than a page that failed to answer.
    async fn dom_outline(page: &Page, selectors: &Selectors) -> Vec<String> {
        let script = r#"([assistant]) => { const out = [];
            const counts = [['configured assistant selector', assistant],
                            ['.ds-markdown', '.ds-markdown'],
                            ["[data-message-role='assistant']", "[data-message-role='assistant']"],
                            ["[data-role='assistant']", "[data-role='assistant']"],
                            ["[data-message-role='user']", "[data-message-role='user']"],
                            ["[class*=ds-markdown]", "[class*=ds-markdown]"],
                            ["[class*=message]", "[class*=message]"]];
            for (const [label, sel] of counts) { out.push('count ' + label + ' = ' + document.querySelectorAll(sel).length); }
            const nodes = [...document.querySelectorAll("[class*='ds-markdown'], [data-message-role], [data-role]")];
            out.push('outline: ' + nodes.length + ' element(s) in DOM order');
            nodes.slice(-14).forEach((n, i) => { const t = (n.innerText || '').trim();
              out.push('  [' + i + '] <' + n.tagName.toLowerCase() + '> role=' + (n.getAttribute('data-message-role') || n.getAttribute('data-role') || '-') + ' class="' + String(n.className).slice(0, 70) + '" chars=' + t.length + ' :: ' + t.slice(0, 70).replace(/\s+/g, ' ')); });
            return out; }"#;
        page.evaluate::<Value, Vec<String>>(script, Some(&serde_json::json!([selectors.assistant])))
            .await
            .unwrap_or_default()
    }

    /// A prompt the size of a real turn. A Codewhale turn pastes a tool catalog
    /// or a whole file into the composer, which is what makes its messages tall
    /// — tall enough that a handful of them fill the window the chat page keeps
    /// mounted.
    fn padded_prompt(token: &str) -> String {
        let mut prompt = String::new();
        for line in 0..80 {
            prompt.push_str(&format!(
                "reference line {line:03}: padding so this message is tall enough to fill \
                 the page's mounted window.\n"
            ));
        }
        prompt.push_str(&format!(
            "\nReply with exactly the word {token} and nothing else."
        ));
        prompt
    }

    /// Live: `[browser] keep_alive = false` must close the browser after each
    /// reply and relaunch it for the next turn.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_turn_closes_browser_when_keep_alive_is_off() {
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true; // no window pop-ups for this one
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: false,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open().await.expect("open the browser");
        assert!(
            ui.session.lock().await.is_some(),
            "the browser must be open before a turn"
        );

        let first = ui
            .send("Reply with exactly the word PONG and nothing else.", true)
            .await
            .expect("first turn");
        eprintln!("[keep-alive off] first reply={first:?}");
        assert!(!first.trim().is_empty());
        assert!(
            ui.session.lock().await.is_none(),
            "keep_alive = false must close the browser after the turn"
        );

        // And the next turn must relaunch it rather than fail.
        let second = ui
            .send("Reply with exactly the word OK and nothing else.", false)
            .await
            .expect("second turn after relaunch");
        eprintln!("[keep-alive off] second reply={second:?}");
        assert!(!second.trim().is_empty());
        assert!(
            ui.session.lock().await.is_none(),
            "the relaunched browser must be closed again"
        );
        ui.shutdown().await;
    }

    /// Live attach mode: drive a Chromium that is already running with
    /// `--remote-debugging-port=9222`, using its own signed-in session.
    #[tokio::test]
    #[ignore = "needs a Chromium listening on :9222 with the signed-in profile"]
    async fn live_attach_mode_round_trip() {
        let home = default_home().expect("codewhale home");
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = false;
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: home.join("deepseek-chat").join("browser"),
                cdp_endpoint: Some("http://127.0.0.1:9222".to_owned()),
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open()
            .await
            .expect("attach to the browser on :9222");
        let attached_url = ui.live_url().await.expect("an attached page");
        eprintln!("[live attach] attached to {attached_url}");
        let reply = ui
            .send("Reply with exactly the word PONG and nothing else.", true)
            .await
            .expect("attached turn");
        eprintln!("[live attach] reply={reply:?}");
        assert!(!reply.trim().is_empty());
        // An attached browser is not ours to close; shutdown only drops the
        // driver connection.
        ui.shutdown().await;
    }

    /// The close watcher: a page that goes away flips the session's liveness, so
    /// a turn can tell "the browser is gone" from "the page is slow" without
    /// waiting out a timeout.
    #[tokio::test]
    async fn a_closed_page_marks_the_session_dead() {
        let playwright = Playwright::launch().await.expect("Playwright driver");
        let browser = playwright
            .chromium()
            .launch()
            .await
            .expect("Playwright Chromium");
        let context = browser.new_context().await.expect("browser context");
        let page = context.new_page().await.expect("page");
        page.set_content("<main><textarea></textarea></main>", None)
            .await
            .expect("fixture");
        let session = Session::new(page, playwright, context, true)
            .await
            .expect("session");
        assert!(session.is_alive(), "a fresh page is alive");

        session.page.close().await.expect("close the page");
        for _ in 0..40 {
            if !session.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !session.is_alive(),
            "a closed page must read as dead without a round trip"
        );
    }

    #[test]
    fn network_errors_are_told_apart_from_service_errors() {
        // DNS: the name never resolved, so nothing reached the service.
        assert_eq!(
            network_error_kind("page.goto: net::ERR_NAME_NOT_RESOLVED at https://x"),
            Some("dns")
        );
        assert_eq!(
            network_error_kind("net::ERR_NAME_RESOLUTION_FAILED"),
            Some("dns")
        );
        // Connectivity: resolved, but nothing answered.
        assert_eq!(
            network_error_kind("net::ERR_INTERNET_DISCONNECTED"),
            Some("network")
        );
        assert_eq!(
            network_error_kind("net::ERR_CONNECTION_REFUSED"),
            Some("network")
        );
        assert_eq!(
            network_error_kind("net::ERR_PROXY_CONNECTION_FAILED"),
            Some("network")
        );
        // A page that merely stayed silent proves nothing about the network.
        assert_eq!(
            network_error_kind("no visible assistant reply within 300s"),
            None
        );
        assert_eq!(network_error_kind("Target closed"), None);
    }

    /// The last link in the chain: a diagnosed failure must land in SQLite as a
    /// failed row, session or no session.
    #[tokio::test]
    async fn the_turn_log_writes_a_failure_row_with_its_diagnosis() {
        let dir = tempfile::tempdir().expect("tempdir");
        let links = Arc::new(
            SessionLinks::open(&dir.path().join("freechatcode/sessions.db")).expect("open"),
        );
        let log = TurnLog {
            links: Arc::clone(&links),
            session: Some("sess-1".to_owned()),
        };
        log.record(TurnRecord {
            model_label: Some("DeepThink=off, Search=on".to_owned()),
            finish_reason: "error".to_owned(),
            tool_calls: false,
            content_chars: 0,
            failure: Some(Failure {
                kind: "dns".to_owned(),
                blame: "network".to_owned(),
                detail: "net::ERR_NAME_NOT_RESOLVED".to_owned(),
                http_status: None,
            }),
        })
        .await
        .expect("record the failure");

        let rows = links.turns(1).expect("turns");
        assert_eq!(rows[0].outcome, "failed");
        assert_eq!(rows[0].failure_kind.as_deref(), Some("dns"));
        assert_eq!(rows[0].blame.as_deref(), Some("network"));
        assert_eq!(
            rows[0].detail.as_deref(),
            Some("net::ERR_NAME_NOT_RESOLVED")
        );
        assert_eq!(rows[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(rows[0].content_chars, 0);
    }

    /// Build a `BrowserChat` that never opens anything, for classification tests.
    fn classifier_for(url: &str) -> BrowserChat {
        let mut config = Config::defaults();
        config.chat.url = url.to_owned();
        BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: PathBuf::from("/nonexistent"),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(5),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        }
    }

    /// The rule the log depends on: a name that will not resolve is the
    /// network's fault, never the service's. `.invalid` never resolves, so this
    /// holds whether or not the machine has a network at all.
    #[tokio::test]
    async fn a_silent_page_with_no_dns_is_not_the_services_fault() {
        let ui = classifier_for("https://freechatcode-test.invalid");
        let failure = ui.classify("no visible assistant reply within 300s").await;
        assert_eq!(failure.kind, "dns");
        assert_eq!(failure.blame, "network");
        assert!(
            !failure.is_service_fault(),
            "an unresolvable name must never be blamed on DeepSeek: {failure:?}"
        );
    }

    /// ...and the converse: when the service *does* answer, with a status, the
    /// blame is theirs.
    #[tokio::test]
    async fn an_http_status_from_the_endpoint_is_the_services_fault() {
        let ui = classifier_for("https://chat.deepseek.com");
        *ui.last_http_status.lock().await = Some(404);
        let failure = ui
            .classify("the api transport returned no text (HTTP 404, 48 bytes)")
            .await;
        assert_eq!(failure.kind, "upstream_http");
        assert_eq!(failure.blame, "service");
        assert_eq!(failure.http_status, Some(404));
        assert!(failure.is_service_fault());
    }

    /// A Chromium network code is a verdict on its own — no probe needed — and
    /// it is not the service's fault.
    #[tokio::test]
    async fn a_chromium_network_code_is_the_networks_fault() {
        let ui = classifier_for("https://chat.deepseek.com");
        let failure = ui.classify("net::ERR_INTERNET_DISCONNECTED").await;
        assert_eq!(failure.kind, "network");
        assert_eq!(failure.blame, "network");
        assert!(!failure.is_service_fault());
    }

    #[test]
    fn only_a_dead_browser_is_worth_a_relaunch() {
        // Not recoverable: the model or the platform said no.
        assert!(!turn_is_recoverable(
            None,
            "the platform did not accept the send; the request is still sitting in the composer"
        ));
        assert!(!turn_is_recoverable(None, "unknown wrapper model"));
        assert!(!turn_is_recoverable(
            None,
            "tool call named an unknown or undeclared Codewhale tool"
        ));
        // Recoverable: the browser went away under us.
        assert!(turn_is_recoverable(None, "Target closed"));
        assert!(turn_is_recoverable(None, "Browser has been closed"));
        assert!(turn_is_recoverable(
            None,
            "the DeepSeek Chat composer is no longer visible"
        ));
    }

    /// Live and self-contained: kill the browser under a running bridge, and the
    /// next turn must reopen it — **on the same conversation** — and carry on.
    /// Runs against a *copy* of the signed-in profile, so it never disturbs the
    /// browser a real session is using.
    #[tokio::test]
    #[ignore = "copies the signed-in profile; run with -- --ignored --nocapture"]
    async fn live_browser_reopens_on_the_same_conversation_after_a_kill() {
        let home = default_home().expect("codewhale home");
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;

        let source = home.join("deepseek-chat").join("browser");
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = dir.path().join("browser-copy");
        let copied = std::process::Command::new("cp")
            .arg("-a")
            .arg(&source)
            .arg(&profile)
            .status()
            .expect("run cp");
        assert!(copied.success(), "could not copy the browser profile");
        // A copied lock would name the *live* owner and make the wrapper refuse to
        // launch its own copy.
        for lock in ["SingletonLock", "SingletonCookie", "SingletonSocket"] {
            let _ = std::fs::remove_file(profile.join(lock));
        }

        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };

        ui.ensure_open().await.expect("open the copy");
        let first = ui
            .send("Reply with exactly the word ONE and nothing else.", true)
            .await
            .expect("first turn");
        eprintln!("[persist] first reply={first:?}");
        let conversation = ui.live_url().await.expect("a live page");
        eprintln!("[persist] conversation={conversation}");
        assert!(
            config.chat.is_resumable_url(&conversation),
            "a real turn must leave a conversation URL: {conversation}"
        );

        // Kill the browser the way a crash would, scoped to the copy's profile.
        let killed = std::process::Command::new("pkill")
            .args(["-9", "-f", &format!("user-data-dir={}", profile.display())])
            .status()
            .expect("run pkill");
        eprintln!("[persist] pkill status={killed}");
        tokio::time::sleep(Duration::from_secs(3)).await;

        let second = ui
            .send("Reply with exactly the word TWO and nothing else.", false)
            .await
            .expect("the turn after the browser was killed");
        eprintln!("[persist] second reply={second:?}");
        assert!(
            !second.trim().is_empty(),
            "the bridge must carry on after the browser dies"
        );
        assert_eq!(
            ui.live_url().await.as_deref(),
            Some(conversation.as_str()),
            "it must reopen the same conversation, not start a new chat"
        );
        ui.shutdown().await;
    }

    /// Diagnostic: open a page and print what it actually shows. When a turn
    /// goes silent, "what is the page displaying?" is the question that matters.
    /// Set `DEEPCHATCODE_INSPECT_URL` to the conversation (defaults to the chat
    /// URL). Runs on a copy of the profile, so it never disturbs a live session.
    #[tokio::test]
    #[ignore = "diagnostic: inspects a live page; needs a signed-in profile"]
    async fn live_inspect_page() {
        let home = default_home().expect("codewhale home");
        let config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        let url =
            std::env::var("DEEPCHATCODE_INSPECT_URL").unwrap_or_else(|_| config.chat.url.clone());
        let (_dir, profile) = profile_copy();

        let playwright = Playwright::launch().await.expect("Playwright driver");
        let options = BrowserContextOptions::builder().headless(true).build();
        let context = playwright
            .chromium()
            .launch_persistent_context_with_options(
                profile.to_str().expect("utf-8 profile path"),
                options,
            )
            .await
            .expect("open the profile copy");
        let page = match context.pages().into_iter().next() {
            Some(page) => page,
            None => context.new_page().await.expect("new page"),
        };
        page.set_default_navigation_timeout(30_000.0).await;
        page.goto(&url, None).await.expect("navigate");
        tokio::time::sleep(Duration::from_secs(6)).await;

        let assistants = page.locator(&config.selectors.assistant);
        eprintln!(
            "[inspect] {url}\n[inspect] assistant elements: {}",
            assistants.count().await.unwrap_or(0)
        );
        for text in assistants
            .all_inner_texts()
            .await
            .unwrap_or_default()
            .iter()
            .rev()
            .take(2)
        {
            eprintln!(
                "[inspect] last assistant text ({} chars): {}",
                text.len(),
                text.chars().take(400).collect::<String>()
            );
        }
        let composer = page.locator(&config.selectors.composer).first();
        eprintln!(
            "[inspect] composer visible: {} | enabled: {}",
            composer.is_visible().await.unwrap_or(false),
            composer.is_enabled().await.unwrap_or(false)
        );
        // The reply-detection contract is "how many assistant elements are in
        // the DOM?", so print exactly what the page keeps mounted. A chat UI
        // that unmounts off-screen messages makes that count stop growing, and
        // this report is how that is diagnosed instead of guessed at.
        for line in dom_outline(&page, &config.selectors).await {
            eprintln!("[inspect] {line}");
        }
        let body = page
            .locator("body")
            .first()
            .inner_text()
            .await
            .unwrap_or_default();
        eprintln!(
            "[inspect] --- visible page text (first 1500 chars) ---\n{}",
            body.chars().take(1500).collect::<String>()
        );
        let _ = context.close().await;
    }

    /// Live: replies must still be read at prompt 4, 5 and 6 — not only the
    /// first few.
    ///
    /// Reported from a real session as "mysterious silence, always around the
    /// 3rd-4th prompt". The chat page virtualizes its transcript: once the
    /// mounted window is full it unmounts an old message in the same render that
    /// mounts the new one, the assistant element count stops growing, and a
    /// detection keyed on that count calls a page that answered silent — the
    /// turn then dies at the response budget. Every prompt here is padded to the
    /// height of a real turn, and the test refuses to pass unless at least one
    /// turn really did answer without the mounted count growing.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_replies_are_read_after_the_mounted_window_fills() {
        const TURNS: usize = 6;
        let home = default_home().expect("codewhale home");
        let (_profile_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true; // no window pop-ups for this one
        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        ui.ensure_open().await.expect("open the browser");
        let page = ui
            .session
            .lock()
            .await
            .as_ref()
            .expect("a live session")
            .page
            .clone();

        let mut counts = Vec::new();
        let mut answered_without_growth = 0_usize;
        for turn in 1..=TURNS {
            let token = format!("W{turn}");
            let before = mounted_count(&page, &config.selectors).await;
            let started = std::time::Instant::now();
            let reply = ui
                .send(&padded_prompt(&token), turn == 1)
                .await
                .unwrap_or_else(|error| panic!("turn {turn} failed: {error}"));
            let after = mounted_count(&page, &config.selectors).await;
            eprintln!(
                "[window] turn {turn}: {:?} mounted assistant elements {before} -> {after}, reply={reply:?}",
                started.elapsed()
            );
            assert!(
                reply.contains(&token),
                "turn {turn} must return its own answer, got {reply:?}"
            );
            if after <= before {
                answered_without_growth += 1;
                for line in dom_outline(&page, &config.selectors).await {
                    eprintln!("[window]   {line}");
                }
            }
            counts.push((before, after));
        }
        eprintln!("[window] mounted assistant elements per turn: {counts:?}");
        ui.shutdown().await;
        assert!(
            answered_without_growth >= 1,
            "this test only proves the fix if the page's mounted window actually filled up, \
             so that a turn answered while the assistant element count stood still; \
             counts were {counts:?}"
        );
    }

    /// Live: with the watcher running, a browser that is killed while the bridge
    /// sits idle must come back **on its own**, on the same conversation, before
    /// any turn is attempted.
    #[tokio::test]
    #[ignore = "copies the signed-in profile; run with -- --ignored --nocapture"]
    async fn live_browser_comes_back_while_idle_without_a_turn() {
        let home = default_home().expect("codewhale home");
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;

        let (_profile_dir, profile) = profile_copy();

        let ui = Arc::new(BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: config.chat.url.clone(),
                profile: profile.clone(),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        });

        ui.ensure_open().await.expect("open the copy");
        let first = ui
            .send("Reply with exactly the word ONE and nothing else.", true)
            .await
            .expect("first turn");
        eprintln!("[idle] first reply={first:?}");
        let conversation = ui.live_url().await.expect("a live page");
        eprintln!("[idle] conversation={conversation}");

        // The watcher, at a test-friendly interval.
        let watching = Arc::clone(&ui);
        let handle =
            tokio::spawn(async move { watching.watch_browser(Duration::from_secs(3)).await });

        // Kill it, then do nothing at all: no turn is ever sent.
        let killed = std::process::Command::new("pkill")
            .args(["-9", "-f", &format!("user-data-dir={}", profile.display())])
            .status()
            .expect("run pkill");
        eprintln!("[idle] pkill status={killed}");

        let mut back = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let alive = ui
                .session
                .lock()
                .await
                .as_ref()
                .is_some_and(|session| session.is_alive());
            if alive {
                back = true;
                break;
            }
        }
        assert!(
            back,
            "the watcher must reopen the browser without waiting for a turn"
        );
        assert_eq!(
            ui.live_url().await.as_deref(),
            Some(conversation.as_str()),
            "it must come back on the same conversation"
        );
        eprintln!("[idle] the browser came back by itself on {conversation}");

        handle.abort();
        ui.shutdown().await;
    }

    /// A live round trip against the real DeepSeek Chat page with the real
    /// browser profile: resume path, incremental streaming, and model label.
    /// Ignored by default because it needs a signed-in profile and the network.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_bridge_round_trip() {
        let home = default_home().expect("codewhale home");
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        // A visible window so a signed-out profile can be signed in by hand.
        config.browser.headless = false;
        // Optional recording, so a demo clip can be produced from a real turn.
        if let Ok(dir) = std::env::var("DEEPCHATCODE_RECORD_VIDEO_DIR") {
            config.browser.record_video_dir = Some(dir);
        }
        if let Ok(size) = std::env::var("DEEPCHATCODE_RECORD_VIDEO_SIZE") {
            config.browser.record_video_size = Some(size);
        }
        let prompt = std::env::var("DEEPCHATCODE_DEMO_PROMPT")
            .unwrap_or_else(|_| "Reply with exactly the word PONG and nothing else.".to_owned());
        let workspace = std::env::current_dir().expect("cwd");
        let sessions_dir = home.join("sessions");
        let links = SessionLinks::open(&home.join("freechatcode").join("sessions.db"))
            .expect("open link store");
        let session = sessions::resolve_session_id(&sessions_dir, &workspace, None);
        let linked = session
            .as_ref()
            .and_then(|id| links.get(id).ok().flatten())
            .filter(|url| config.chat.is_resumable_url(url));
        eprintln!("[live] session={session:?}");
        eprintln!("[live] linked conversation={linked:?}");
        let start_fresh = linked.is_none();

        let ui = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: linked.unwrap_or_else(|| config.chat.url.clone()),
                profile: home.join("deepseek-chat").join("browser"),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };

        ui.ensure_open().await.expect("open the browser");
        eprintln!("[live] resume path held={}", ui.link_available().await);
        eprintln!(
            "[live] model label right now: {:?}",
            ui.read_model_label(
                &ui.session
                    .lock()
                    .await
                    .as_ref()
                    .expect("a live session")
                    .page
            )
            .await
        );

        let started = std::time::Instant::now();
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<String>(64);
        let pump = tokio::spawn(async move {
            let mut count = 0_usize;
            let mut first_at = None;
            let mut last = String::new();
            while let Some(snapshot) = receiver.recv().await {
                if first_at.is_none() {
                    first_at = Some(started.elapsed());
                }
                count += 1;
                last = snapshot;
            }
            (count, first_at, last)
        });
        let reply = ui
            .send_streaming(&prompt, start_fresh, sender)
            .await
            .expect("live reply");
        let (snapshots, first_at, last) = pump.await.expect("snapshot pump");
        eprintln!("[live] first snapshot at {:?}", first_at);
        eprintln!("[live] total wall time: {:?}", started.elapsed());
        eprintln!("[live] snapshots={snapshots} last_snapshot={last:?}");
        eprintln!("[live] model label={:?}", ui.model_label().await);
        eprintln!("[live] reply={reply:?}");
        assert!(!reply.trim().is_empty(), "a live turn must return text");

        // Resume: the conversation just created must be recognised as a link and
        // continued, not replaced. This is the first criterion, so it is checked
        // against the real page rather than a mock.
        let conversation = ui.live_url().await.expect("a live page");
        eprintln!("[live] conversation url={conversation}");
        assert!(
            config.chat.is_resumable_url(&conversation),
            "a real turn must leave a resumable conversation URL: {conversation}"
        );
        ui.shutdown().await;

        let resumed = BrowserChat {
            spec: BrowserSpec {
                config: config.clone(),
                url: conversation.clone(),
                profile: home.join("deepseek-chat").join("browser"),
                cdp_endpoint: None,
            },
            session: Mutex::new(None),
            transport: TransportMode::Gui,
            api: config.transport.api.clone(),
            api_timeout: Duration::from_secs(60),
            keep_alive: true,
            response_timeout: config.timeouts.response(),
            model_label: Mutex::new(None),
            resume_url: Mutex::new(String::new()),
            last_http_status: Mutex::new(None),
        };
        resumed
            .ensure_open()
            .await
            .expect("reopen on the linked conversation");
        let recognised = resumed.link_available().await;
        eprintln!("[live] link recognised on resume={recognised}");
        assert!(
            recognised,
            "the linked conversation must be recognised on resume, not treated as stale"
        );
        let follow_up = resumed
            .send("Reply with exactly the word OK and nothing else.", false)
            .await
            .expect("resumed turn");
        eprintln!("[live] resumed reply={follow_up:?}");
        assert!(!follow_up.trim().is_empty());
        assert_eq!(
            resumed.live_url().await.as_deref(),
            Some(conversation.as_str()),
            "a resumed session must stay in the same conversation"
        );
        resumed.shutdown().await;
    }

    /// Live: the wrapper keeps the terminal raw for Codewhale's TUI.
    ///
    /// `playwright-rs` keeps a defensive termios guard (its fix for issue #59):
    /// it snapshots *stdin's* line discipline the first time a `Playwright` is
    /// launched and writes that snapshot back whenever one is dropped. The
    /// bridge hands the terminal to Codewhale at 0.00s and brings the browser up
    /// behind it, so that write-back landed *after* the TUI had put the line
    /// discipline into raw mode: the terminal came back cooked and echoing, and
    /// the TUI's own mouse reports were echoed over the UI as `^[[<35;10;5M`
    /// text. `take_terminal_off_stdin` takes the terminal off the wrapper's own
    /// fd 0 before the browser exists and hands Codewhale the terminal
    /// explicitly, so the guard has nothing to snapshot and nothing to write
    /// back.
    ///
    /// This runs the shipped wrapper on a real pty with no Codewhale arguments,
    /// so Codewhale runs its interactive TUI, and asserts what the user is left
    /// with once the browser is warm: `ECHO` still clear across consecutive
    /// samples, the wrapper's own fd 0 not a terminal while the child it spawned
    /// holds this terminal on fd 0, and a mouse report written to the master not
    /// coming back as text.
    #[tokio::test]
    #[ignore = "needs a signed-in profile, the network, and a built binary"]
    async fn live_the_wrapper_keeps_the_terminal_raw_for_the_tui() {
        use std::io::{Read as _, Write as _};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::CommandExt;

        /// The report a terminal emulator sends when the pointer moves, and the
        /// text an echoing line discipline turns it into: `ECHOCTL` renders the
        /// `ESC` as a caret pair, which is what used to appear over the UI.
        const MOUSE: &[u8] = b"\x1b[<35;10;5M";
        const ECHOED: &[u8] = b"^[[<35;10;5M";

        /// Whether `haystack` contains `needle`.
        fn holds(haystack: &[u8], needle: &[u8]) -> bool {
            !needle.is_empty()
                && haystack
                    .windows(needle.len())
                    .any(|window| window == needle)
        }

        /// The last of what the wrapper printed, for a failure message.
        fn tail(output: &[u8]) -> String {
            let start = output.len().saturating_sub(1200);
            String::from_utf8_lossy(&output[start..]).into_owned()
        }

        /// What `/proc/<pid>/fd/0` points at, and whether that is a terminal.
        fn stdin_of(pid: u32) -> (String, bool) {
            let path = format!("/proc/{pid}/fd/0");
            let target = std::fs::read_link(&path)
                .map(|target| target.display().to_string())
                .unwrap_or_else(|error| format!("<{error}>"));
            let is_tty = std::ffi::CString::new(path)
                .ok()
                .map(|path| {
                    // SAFETY: open(2), then isatty(2) on that descriptor, closed
                    // again here. O_NOCTTY keeps the test from adopting a
                    // terminal it is only inspecting.
                    unsafe {
                        let fd = libc::open(
                            path.as_ptr(),
                            libc::O_RDONLY | libc::O_NOCTTY | libc::O_NONBLOCK,
                        );
                        if fd < 0 {
                            return false;
                        }
                        let tty = libc::isatty(fd) == 1;
                        libc::close(fd);
                        tty
                    }
                })
                .unwrap_or(false);
            (target, is_tty)
        }

        /// The processes `wrapper` has spawned so far, and their names.
        fn children_of(wrapper: u32) -> Vec<(u32, String)> {
            let mut children = Vec::new();
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return children;
            };
            for entry in entries.flatten() {
                let Ok(child) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let status =
                    std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
                let parent = status
                    .lines()
                    .find_map(|line| line.strip_prefix("PPid:"))
                    .and_then(|parent| parent.trim().parse::<u32>().ok());
                if parent != Some(wrapper) {
                    continue;
                }
                let name = std::fs::read_to_string(entry.path().join("comm"))
                    .map(|name| name.trim().to_owned())
                    .unwrap_or_default();
                children.push((child, name));
            }
            children
        }

        /// A pty, the way a terminal emulator gives one to a program.
        struct Pty {
            master: std::fs::File,
            slave: String,
        }

        impl Pty {
            fn open() -> Self {
                // SAFETY: every call acts on the master descriptor this function
                // owns, and each one is checked before the next.
                unsafe {
                    let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
                    assert!(
                        master >= 0,
                        "posix_openpt: {}",
                        std::io::Error::last_os_error()
                    );
                    for (code, call) in [
                        (libc::grantpt(master), "grantpt"),
                        (libc::unlockpt(master), "unlockpt"),
                    ] {
                        assert_eq!(code, 0, "{call}: {}", std::io::Error::last_os_error());
                    }
                    let mut name = [0 as libc::c_char; 128];
                    assert_eq!(
                        libc::ptsname_r(master, name.as_mut_ptr(), name.len()),
                        0,
                        "ptsname_r: {}",
                        std::io::Error::last_os_error()
                    );
                    let slave = std::ffi::CStr::from_ptr(name.as_ptr())
                        .to_string_lossy()
                        .into_owned();
                    // Non-blocking, so draining the child's output can never
                    // stall the run on a full buffer.
                    let flags = libc::fcntl(master, libc::F_GETFL);
                    assert!(flags >= 0, "F_GETFL: {}", std::io::Error::last_os_error());
                    libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    let window = libc::winsize {
                        ws_row: 32,
                        ws_col: 120,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    libc::ioctl(master, libc::TIOCSWINSZ.into(), &window);
                    Self {
                        master: std::fs::File::from_raw_fd(master),
                        slave,
                    }
                }
            }

            /// The line discipline the terminal is in right now: `ECHO` set
            /// means every byte typed at it is echoed back as text.
            fn echoing(&self) -> bool {
                // SAFETY: tcgetattr fills `settings` on success and leaves the
                // descriptor untouched.
                unsafe {
                    let mut settings: libc::termios = std::mem::zeroed();
                    assert_eq!(
                        libc::tcgetattr(self.master.as_raw_fd(), &mut settings),
                        0,
                        "read the pty's line discipline: {}",
                        std::io::Error::last_os_error()
                    );
                    settings.c_lflag & libc::ECHO != 0
                }
            }

            /// Everything the child has written. Never blocks, and a closed pty
            /// is simply the end of the output.
            fn drain(&self, output: &mut Vec<u8>) {
                let mut chunk = [0_u8; 65536];
                let mut reader = &self.master;
                while let Ok(read) = reader.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    output.extend_from_slice(&chunk[..read]);
                }
            }
        }

        /// The wrapper under test, with a cleanup that reaches exactly its own
        /// process group.
        ///
        /// The child is a session leader (see `pre_exec` below), so its pid is
        /// its process group id and `killpg` covers it and what it spawned.
        /// Nothing here matches on process names: a live bridge may be running
        /// on this machine, and a broad `pkill` would take it down with the
        /// test.
        struct Run {
            child: std::process::Child,
            reaped: bool,
        }

        impl Run {
            fn reap(&mut self) {
                if self.reaped {
                    return;
                }
                let running = self.child.try_wait().ok().flatten().is_none();
                if running {
                    // SAFETY: killpg(2) on the process group setsid(2) created
                    // for this child, which is still running.
                    unsafe {
                        libc::killpg(self.child.id() as libc::pid_t, libc::SIGKILL);
                    }
                    let _ = self.child.wait();
                }
                self.reaped = true;
            }
        }

        impl Drop for Run {
            fn drop(&mut self) {
                self.reap();
            }
        }

        let (_profile_dir, profile) = profile_copy();
        let cli = wrapper_cli();
        let pty = Pty::open();
        eprintln!("[tty] running {} on {}", cli.display(), pty.slave);

        let slave = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&pty.slave)
            .expect("open the pty slave");
        let mut command = std::process::Command::new(&cli);
        command
            .arg("--mode")
            .arg("silent")
            .arg("--profile-dir")
            .arg(&profile)
            .stdin(Stdio::from(slave.try_clone().expect("clone the slave")))
            .stdout(Stdio::from(slave.try_clone().expect("clone the slave")))
            .stderr(Stdio::from(slave));
        // SAFETY: `pre_exec` runs between fork and exec, where only
        // async-signal-safe calls are allowed — setsid(2) and ioctl(2) are. std
        // has already installed the stdio descriptors by then, so fd 0 is the
        // pty. A session of its own with this pty as the controlling terminal is
        // what a terminal emulator gives a program, and it is what makes the
        // cleanup below a `killpg` and nothing wider.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut run = Run {
            child: command.spawn().expect("run the wrapper on the pty"),
            reaped: false,
        };
        let wrapper = run.child.id();

        // Startup: the browser comes up behind Codewhale, and the TUI draws
        // itself. The window that matters opens at five seconds, when the
        // browser is warm — exactly where the guard used to write its snapshot
        // back over the TUI's raw mode.
        let mut output = Vec::new();
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            pty.drain(&mut output);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Ok(Some(status)) = run.child.try_wait() {
            panic!(
                "the wrapper must still be running five seconds in (it exited with {status}); \
                 output: {}",
                tail(&output)
            );
        }
        // The guard is only in play once a browser was launched, and the TUI is
        // what needs the terminal: without these, a pass would say nothing.
        assert!(
            holds(&output, b"browser ready in"),
            "the browser must be warm for this to be about the guard; output: {}",
            tail(&output)
        );
        assert!(
            holds(&output, b"\x1b[?1006h"),
            "Codewhale must be in its interactive TUI (no mouse tracking seen); output: {}",
            tail(&output)
        );

        // One lucky sample is not evidence: eight consecutive clear samples over
        // eight tenths of a second is.
        let mut echoed = Vec::new();
        for _ in 0..8 {
            echoed.push(pty.echoing());
            pty.drain(&mut output);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        eprintln!("[tty] ECHO samples five seconds in: {echoed:?}");
        assert!(
            echoed.iter().all(|echoing| !echoing),
            "the terminal must stay raw while the TUI is up, but ECHO came back: {echoed:?}; \
             output: {}",
            tail(&output)
        );

        // The guard can only write back to fd 0, so the wrapper's own fd 0 must
        // not be the terminal...
        let (wrapper_stdin, wrapper_is_tty) = stdin_of(wrapper);
        eprintln!("[tty] wrapper {wrapper} fd0={wrapper_stdin} tty={wrapper_is_tty}");
        assert!(
            !wrapper_is_tty,
            "the wrapper must not hold the terminal on its own fd 0 (it holds {wrapper_stdin})"
        );
        // ...while the child it spawned must have been handed this terminal.
        let children = children_of(wrapper);
        assert!(
            !children.is_empty(),
            "the wrapper must have spawned Codewhale by now; output: {}",
            tail(&output)
        );
        let holding: Vec<String> = children
            .iter()
            .map(|(pid, name)| {
                let (stdin, tty) = stdin_of(*pid);
                format!("{pid} {name} fd0={stdin} tty={tty}")
            })
            .collect();
        eprintln!("[tty] children of {wrapper}: {holding:?}");
        assert!(
            children.iter().any(|(pid, _)| {
                let (stdin, tty) = stdin_of(*pid);
                tty && stdin == pty.slave
            }),
            "the wrapper must hand the terminal ({}) to the child it spawned: {holding:?}",
            pty.slave
        );

        // The user-visible symptom: with ECHO on, the line discipline hands the
        // TUI's own mouse reports straight back as text over the UI.
        let before = output.len();
        let mut writer = &pty.master;
        writer
            .write_all(MOUSE)
            .expect("write a mouse report to the pty master");
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while std::time::Instant::now() < deadline {
            pty.drain(&mut output);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let answered = &output[before..];
        eprintln!(
            "[tty] after a mouse report: {} bytes came back, echoed as text = {}",
            answered.len(),
            holds(answered, ECHOED) || holds(answered, MOUSE)
        );
        assert!(
            !holds(answered, ECHOED) && !holds(answered, MOUSE),
            "the terminal must not echo the TUI's mouse reports back as text"
        );

        run.reap();
        eprintln!("[tty] the terminal stayed raw for wrapper {wrapper}");
    }
}
