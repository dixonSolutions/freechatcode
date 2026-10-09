//! Chromium management and Playwright chat I/O.
//!
//! `Session` owns the live browser; `BrowserChat` drives one chat page
//! through it — submit, read, stream, recover. Kept separate from the relay
//! (lib.rs), the session-link store (sessions.rs), and config (config.rs).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use playwright_rs::protocol::{
    BrowserContext, BrowserContextOptions, GotoOptions, Page, RecordVideo, Viewport, WaitUntil,
};
use playwright_rs::{ConnectOverCdpOptions, Error as PlaywrightError, Playwright};
use tokio::sync::Mutex;
use tokio::time::Instant;

use freechatcode::config::{
    ApiTransportConfig, BrowserMode, Config, Provider, Selectors, Timeouts, Toggle, TransportMode,
};
use freechatcode::{ChatUi, Failure, extract_api_text};

#[derive(Clone)]
pub(crate) struct BrowserSpec {
    pub(crate) config: Config,
    pub(crate) provider: Provider,
    pub(crate) url: String,
    pub(crate) profile: PathBuf,
    pub(crate) cdp_endpoint: Option<String>,
}

/// A live browser: the page the relay drives plus the handles that keep it up.
pub(crate) struct Session {
    pub(crate) page: Page,
    pub(crate) playwright: Playwright,
    pub(crate) context: BrowserContext,
    /// A wrapper-owned browser is closed on exit; an attached one is not.
    pub(crate) managed: bool,
    /// Flipped by the page's own close/crash handlers, and by its context closing.
    pub(crate) closed: Arc<AtomicBool>,
}

impl Session {
    /// Wrap a live page and start watching for it going away, so a turn can tell
    /// "the browser is gone" from "the page is slow" without a round trip.
    pub(crate) async fn new(
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
    pub(crate) fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::SeqCst) && !self.page.is_closed()
    }
}

/// Register a handler that flips `closed` when the page closes or crashes.
pub(crate) async fn watched_page(
    page: &Page,
    closed: Arc<AtomicBool>,
) -> Result<(), playwright_rs::Error> {
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
pub(crate) async fn watched_context(
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
pub(crate) struct BrowserChat {
    pub(crate) spec: BrowserSpec,
    /// The live browser, held across turns unless `[browser] keep_alive` is off.
    pub(crate) session: Mutex<Option<Session>>,
    pub(crate) transport: TransportMode,
    pub(crate) api: ApiTransportConfig,
    pub(crate) api_timeout: Duration,
    pub(crate) keep_alive: bool,
    pub(crate) response_timeout: Duration,
    /// What the page last said it was using. Recorded per turn.
    pub(crate) model_label: Mutex<Option<String>>,
    /// The conversation this run is in, once one exists. Reopening the browser
    /// returns here rather than starting a fresh chat. Empty means "use the
    /// configured URL".
    pub(crate) resume_url: Mutex<String>,
    /// The HTTP status the `api` transport last saw, if any. A status is proof
    /// the service answered, which is what separates "their fault" from ours.
    pub(crate) last_http_status: Mutex<Option<u16>>,
}

impl BrowserChat {
    pub(crate) fn selectors(&self) -> &Selectors {
        &self.spec.provider.selectors
    }

    pub(crate) fn timeouts(&self) -> &Timeouts {
        &self.spec.config.timeouts
    }

    /// Open the browser the configured way: a managed profile, or attach to an
    /// already-running Chromium.
    pub(crate) async fn open(&self) -> Result<Session> {
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
    pub(crate) async fn attach(&self) -> Result<Session> {
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
                if !self.spec.provider.chat.accepts_url(&page.url()) {
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
    pub(crate) async fn launch_managed(&self) -> Result<Session> {
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
        super::set_private_directory(&self.spec.profile).await?;
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
                    if !self.spec.provider.chat.accepts_url(&page.url()) {
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
    pub(crate) async fn navigate(&self, page: &Page) -> Result<()> {
        let url = self.target_url().await;
        self.navigate_to(page, &url).await
    }

    pub(crate) async fn navigate_to(&self, page: &Page, url: &str) -> Result<()> {
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
    pub(crate) async fn conversation_present(&self, page: &Page) -> bool {
        let chat = &self.spec.provider.chat;
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

    pub(crate) async fn wait_for_composer(&self, page: &Page, timeout: Duration) -> Result<()> {
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
    pub(crate) async fn read_model_label(&self, page: &Page) -> Option<String> {
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
    pub(crate) async fn target_url(&self) -> String {
        let current = self.resume_url.lock().await;
        if current.is_empty() {
            self.spec.url.clone()
        } else {
            current.clone()
        }
    }

    /// Record the conversation the page is on, so a relaunch returns to it
    /// instead of starting a new chat.
    pub(crate) async fn remember_conversation(&self, page: &Page) {
        let url = page.url();
        if self.spec.provider.chat.is_resumable_url(&url) {
            *self.resume_url.lock().await = url;
        }
    }

    /// Guarantee a live, usable page, reopening the browser when it has gone
    /// away. This is the persistence guarantee: a browser that is closed or
    /// crashes costs a relaunch, not the session.
    pub(crate) async fn ensure_live(&self, guard: &mut Option<Session>) -> Result<(), String> {
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
    pub(crate) fn recoverable(&self, session: Option<&Session>, error: &str) -> bool {
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
    pub(crate) async fn classify(&self, error: &str) -> Failure {
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
    pub(crate) async fn probe_reachability(&self) -> Reachability {
        let host = url::Url::parse(&self.spec.provider.chat.url)
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
    pub(crate) async fn turn(
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
    pub(crate) async fn call_api(&self, page: &Page, prompt: &str) -> Result<String, String> {
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

    pub(crate) async fn submit(
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
        let assistants = page.locator(reply_selector(self.selectors()));
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
    pub(crate) async fn start_new_chat(&self, page: &Page) -> Result<(), String> {
        let chat = &self.spec.provider.chat;
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
    pub(crate) async fn ensure_open(&self) -> Result<(), String> {
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
    pub(crate) async fn watch_browser(self: Arc<Self>, interval: Duration) {
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
    pub(crate) async fn link_available(&self) -> bool {
        let guard = self.session.lock().await;
        let Some(session) = guard.as_ref() else {
            return false;
        };
        self.conversation_present(&session.page).await
    }

    /// The live page's URL, when a browser is open.
    pub(crate) async fn live_url(&self) -> Option<String> {
        let guard = self.session.lock().await;
        guard.as_ref().map(|session| session.page.url())
    }

    /// Close the browser for good at the end of the run.
    pub(crate) async fn shutdown(&self) {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.take() {
            close_session(session).await;
        }
    }
}

/// Whether `open` attaches instead of launching. An explicit CDP endpoint always
/// means attach, whatever `mode` says — that is what `--cdp-endpoint` promises.
pub(crate) fn resolves_to_attach(mode: BrowserMode, cdp_endpoint: Option<&str>) -> bool {
    cdp_endpoint.is_some_and(|endpoint| !endpoint.trim().is_empty()) || mode == BrowserMode::Attach
}

/// A profile another Chromium already holds cannot be launched. Detect it up
/// front so the failure is one actionable sentence instead of a node stack.
/// Chromium records the holder in `SingletonLock` as a `"<host>-<pid>"` symlink.
pub(crate) fn profile_holder(profile: &Path) -> Option<String> {
    let target = std::fs::read_link(profile.join("SingletonLock")).ok()?;
    let target = target.to_string_lossy().into_owned();
    let pid: i32 = target.rsplit('-').next()?.parse().ok()?;
    Path::new(&format!("/proc/{pid}"))
        .exists()
        .then_some(target)
}

/// Ceiling for the liveness watcher's retry backoff.
pub(crate) const MAX_LIVENESS_BACKOFF: Duration = Duration::from_secs(300);

/// Whether the chat host can be reached at all, independent of the browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reachability {
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
pub(crate) fn reply_arrived(
    previous_count: usize,
    previous_text: &str,
    count: usize,
    text: &str,
) -> bool {
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

/// The selector the reply is read from: the assistant block, with the page's
/// own **reasoning** excluded.
///
/// On DeepSeek with DeepThink on, the reasoning is rendered as markdown inside a
/// `.ds-think-content` panel that sits above the answer, in the same transcript
/// the `assistant` selector matches (DeepSeek's own stylesheet says so:
/// `.ds-think-content .ds-markdown { … }`). Reading the newest match without this
/// exclusion hands the harness the page's thinking as the model's answer, and
/// turns a harness session title into the first line of that thinking.
///
/// The reasoning is not part of what the wrapper delivers: it is ignored, not
/// translated (a harness that wants thinking has its own channel for it, and this
/// text is the page's readout of a model the harness did not call). The
/// exclusion is applied to every alternative in the selector list, so a
/// comma-separated `assistant` keeps working.
pub(crate) fn reply_selector(selectors: &Selectors) -> String {
    let assistant = selectors.assistant.trim();
    let reasoning = selectors.reasoning.trim();
    if reasoning.is_empty() {
        return assistant.to_owned();
    }
    assistant
        .split(',')
        .map(|one| format!("{}:not({reasoning}):not({reasoning} *)", one.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Classify a browser-level network error from its Chromium code.
pub(crate) fn network_error_kind(error: &str) -> Option<&'static str> {
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
pub(crate) async fn chip_is_selected(page: &Page, selector: &str) -> Option<bool> {
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
pub(crate) fn first_line(error: &str) -> String {
    error.lines().next().unwrap_or(error).trim().to_owned()
}

/// Whether a failed turn is worth one relaunch. A browser that has gone away —
/// or a page whose composer vanished under it — is recoverable; a refusal from
/// the model is not.
pub(crate) fn turn_is_recoverable(session: Option<&Session>, error: &str) -> bool {
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
pub(crate) async fn close_session(session: Session) {
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

    /// Make the page's state match the model this turn asked for.
    ///
    /// Declarative, not a toggle: a model is page state (a reasoning chip, a
    /// dropdown), so a pro turn must not leak into the chat turn after it and a
    /// chat turn must not inherit a pro one. A model that cannot engage a
    /// required control fails — answering with the plain model while the turn
    /// log says pro would be a lie.
    async fn set_model_state(&self, toggles: &[Toggle]) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        let session = guard.as_mut().ok_or("the browser is not open")?;
        let page = &session.page;
        for toggle in toggles {
            let selector = toggle.selector.trim().to_owned();
            if selector.is_empty() {
                if toggle.on {
                    return Err("a model toggle has an empty selector".to_owned());
                }
                continue;
            }
            let control = page.locator(&selector).first();
            if !control.is_visible().await.unwrap_or(false) {
                if toggle.on {
                    return Err(format!(
                        "no visible control on the page ({selector}); check the provider's toggle selectors"
                    ));
                }
                // Nothing to settle: a page without the control is already "off".
                continue;
            }
            let engaged = chip_is_selected(page, &selector).await;
            if engaged == Some(toggle.on) {
                continue;
            }
            tokio::time::timeout(self.timeouts().action(), control.click(None))
                .await
                .map_err(|_| format!("clicking the control ({selector}) timed out"))?
                .map_err(|error| format!("could not click the control ({selector}): {error}"))?;
            // A click is a request, not a fact. Read it back before believing it.
            let after = chip_is_selected(page, &selector).await;
            if after != Some(toggle.on) {
                return Err(format!(
                    "the control ({selector}) did not {}{}",
                    if toggle.on { "engage" } else { "disengage" },
                    match after {
                        Some(_) => " (it read back unchanged)",
                        None => " (its state could not be read back)",
                    }
                ));
            }
        }
        Ok(())
    }
}

/// Build the browser driver for one provider, resolving its profile directory
/// (an explicit override wins, otherwise `~/.codewhale/providers/<id>/browser`).
pub(crate) fn make_browser(
    config: &Config,
    provider: &Provider,
    home: &Path,
    profile_override: Option<PathBuf>,
    cdp_endpoint: Option<String>,
    url: String,
) -> BrowserChat {
    let profile = profile_override
        .unwrap_or_else(|| home.join("providers").join(&provider.id).join("browser"));
    BrowserChat {
        spec: BrowserSpec {
            config: config.clone(),
            provider: provider.clone(),
            url,
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
    }
}

/// Best-effort user notification (a desktop "toast"). Always mirrors to stderr;
/// the desktop popup goes through `notify-send` and is skipped when disabled or
/// unavailable.
pub(crate) fn notify(enabled: bool, summary: &str, body: &str) {
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
