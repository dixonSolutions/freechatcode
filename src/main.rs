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
use clap::{Args as ClapArgs, Parser, Subcommand};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

use freechatcode::config::{self, ChatConfig, Config, Provider, RunMode, TransportMode};
use freechatcode::harness::{self, HarnessKind};
use freechatcode::health;
use freechatcode::sessions::{self, SessionLinks, TurnRow};
use freechatcode::setup;
use freechatcode::{
    AuditSink, BridgeOptions, ChatUi, Failure, ModelSpec, RouteGroup, ServerState, ToolPolicy,
    TurnRecord, TurnSink, router,
};

mod browser;
mod tui;

use browser::{BrowserChat, first_line, make_browser, notify};

#[derive(Debug, Parser)]
#[command(
    name = "freechatcode",
    about = "Run coding agents through free chat web UIs — no API key"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Attach to an already-running Chromium via this CDP endpoint.
    #[arg(long, global = true)]
    cdp_endpoint: Option<String>,

    /// Chat page to open (defaults to the configured chat URL).
    #[arg(long, global = true)]
    chat_url: Option<String>,

    /// Codewhale executable to launch after the browser is ready.
    #[arg(long, alias = "harness-bin", env = "CODEWHALE_BINARY", global = true)]
    codewhale_bin: Option<String>,

    /// Agent harness to launch: Codewhale (default), OpenCode, or OpenClaw. Guessed
    /// from the binary's file name when omitted.
    #[arg(long, value_enum, global = true)]
    harness: Option<HarnessKind>,

    /// Browser profile directory.
    #[arg(long, global = true)]
    profile_dir: Option<PathBuf>,

    /// Record the browser's page to a video in this directory (one file per
    /// run). Useful for demos; it never records the desktop, only the page.
    #[arg(long, global = true)]
    record_video: Option<PathBuf>,

    /// Recording size for --record-video, as WxH.
    #[arg(long, global = true)]
    record_video_size: Option<String>,

    /// How the wrapper presents itself: `show` (a visible browser, prompts
    /// driven through the page) or `silent` (headless, prompts sent to the API
    /// directly). Overrides `mode` in the config file. Sign-in reopens a visible
    /// window in both modes.
    #[arg(long, value_enum, global = true)]
    mode: Option<RunMode>,

    /// Model to launch the harness with. Defaults to the chat model.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Select a provider by name (configures it first when needed).
    #[arg(long, global = true)]
    chatmodel: Option<String>,
    /// Remember the selected provider as the default.
    #[arg(long, global = true)]
    make_default: bool,
    /// Expose all configured models to the harness: --opt=all.
    #[arg(long, global=true, value_parser=["all"], alias="options")]
    opt: Option<String>,

    /// Arguments passed unchanged to the harness after `--`.
    #[arg(last = true, value_name = "HARNESS_ARGS")]
    codewhale_args: Vec<OsString>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// List, configure, remove, or select chat providers.
    Chatmodels {
        #[command(subcommand)]
        command: ChatModelCommands,
    },
    /// File feedback on GitHub; attachments are linked rather than uploaded.
    Feedback {
        #[arg(long)]
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, conflicts_with = "body")]
        body_file: Option<PathBuf>,
    },
    /// Check the harness binary, the relay, and the browser.
    Health,
    /// Install the Codewhale CLI (the default harness) via cargo.
    Install,
    /// Show the pre-flight TUI, then run the bridge.
    Tui,
    /// Print the recorded turn log: which model answered what, per chat.
    Turns {
        /// How many recent turns to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Check each configured provider: browser, composer, sign-in.
    Doctor {
        /// Check only this provider (default: all configured providers).
        #[arg(long)]
        provider: Option<String>,
    },
    /// Find (and remember) the harness binary, then run the bridge.
    Launch {
        /// Optional harness executable name or path to track down.
        binary: Option<String>,
        /// Use the saved default selection (also the default without this flag).
        #[arg(long)]
        default: bool,
        /// Arguments passed unchanged to the harness after `--`.
        #[arg(last = true, value_name = "HARNESS_ARGS")]
        codewhale_args: Vec<OsString>,
    },
}

#[derive(Debug, ClapArgs)]
struct ProviderTarget {
    provider: Option<String>,
    #[arg(long = "Deepseek", alias = "deepseek")]
    deepseek: bool,
    #[arg(long = "Gemini", alias = "gemini")]
    gemini: bool,
    #[arg(long = "GoogleAIMode", alias = "google-ai-mode")]
    google_ai_mode: bool,
    #[arg(long)]
    all: bool,
}
impl ProviderTarget {
    fn name(&self) -> Result<String> {
        let mut names = Vec::new();
        if let Some(name) = &self.provider {
            names.push(name.clone());
        }
        for (enabled, name) in [
            (self.deepseek, "deepseek"),
            (self.gemini, "gemini"),
            (self.google_ai_mode, "google-ai-mode"),
            (self.all, "all"),
        ] {
            if enabled {
                names.push(name.into());
            }
        }
        if names.len() != 1 {
            bail!(
                "select exactly one provider by name or --Deepseek/--Gemini/--GoogleAIMode/--all"
            );
        }
        Ok(names.remove(0))
    }
}
#[derive(Debug, Subcommand)]
enum ChatModelCommands {
    List,
    Configure {
        #[command(flatten)]
        target: ProviderTarget,
    },
    Drop {
        #[command(flatten)]
        target: ProviderTarget,
    },
    ReConfigure {
        #[command(flatten)]
        target: ProviderTarget,
    },
    SetDefault {
        #[command(flatten)]
        target: ProviderTarget,
    },
}

/// One JSONL file per relay run: inputs, outputs, errors, with the path the
/// wrapper already names on startup.
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
    harness: String,
    identity: Option<freechatcode::ConversationIdentity>,
    inner: Arc<BrowserChat>,
    links: Arc<SessionLinks>,
    session: Option<String>,
    sessions_dir: PathBuf,
    workspace: PathBuf,
    chat: ChatConfig,
    provider_id: String,
    /// Set by the startup warm-up when the linked conversation turned out not to
    /// exist. Checked once, on the first turn: the browser is warmed in parallel
    /// with Codewhale's own boot, so this answer arrives after the relay is
    /// already serving.
    stale_link: Arc<AtomicBool>,
}

impl UrlLinkingChat {
    fn record_agent_message(&self, prompt: &str, reply: &str) {
        if let Some(id) = &self.identity
            && let Err(error) = self.links.record_agent_message(
                &id.session_id,
                &id.agent_id,
                &self.provider_id,
                prompt.len(),
                reply.len(),
            )
        {
            eprintln!("freechatcode: could not record agent message: {error}");
        }
    }
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
        let Some(url) = self.inner.live_url().await else {
            return false;
        };
        if !self.chat.is_resumable_url(&url) {
            return false;
        }
        let session = self.session.clone().or_else(|| {
            (self.harness == "codewhale")
                .then(|| sessions::active_session_id(&self.sessions_dir, &self.workspace))
                .flatten()
        });
        let Some(session) = session else {
            return false;
        };
        let previous = if let Some(identity) = &self.identity {
            self.links
                .agent_url(&identity.session_id, &identity.agent_id, &self.provider_id)
        } else {
            self.links.get(&session, &self.provider_id)
        };
        if previous.ok().flatten().as_deref() == Some(&url) {
            return true;
        }
        let result = if let Some(identity) = &self.identity {
            self.links
                .link_agent(
                    &identity.session_id,
                    &identity.agent_id,
                    &self.provider_id,
                    &url,
                )
                .and_then(|()| {
                    if identity.agent_id.is_empty() {
                        self.links
                            .upsert(&identity.session_id, &self.provider_id, &url)
                    } else {
                        Ok(())
                    }
                })
        } else {
            self.links.upsert(&session, &self.provider_id, &url)
        };
        match result {
            Ok(()) => {
                let title = match &self.identity {
                    Some(id) if !id.agent_id.is_empty() => format!(
                        "[{}] sub_agent session_id:{} sub_agent_id:{}",
                        self.harness, id.session_id, id.agent_id
                    ),
                    _ => format!("[{}] session_id:{session}", self.harness),
                };
                if let Err(error) = self.inner.rename_conversation(&title).await {
                    eprintln!("freechatcode: session linked; could not rename chat: {error}");
                }
                eprintln!("Linked {} session {session} to {url}", self.harness);
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
    async fn resuming(&self) -> bool {
        !self.inner.resume_url.lock().await.is_empty()
    }
    async fn prepare_turn(&self, start_new_chat: bool) -> Result<bool, String> {
        self.inner
            .prepare_turn(start_new_chat || self.take_stale_link())
            .await
    }
    async fn fork(
        self: Arc<Self>,
        identity: &freechatcode::ConversationIdentity,
    ) -> Result<Arc<dyn ChatUi>, String> {
        let mut resolved = identity.clone();
        if self.harness == "codewhale"
            && resolved.agent_id.is_empty()
            && resolved.session_id.starts_with("run-")
            && let Some(session) = sessions::active_session_id(&self.sessions_dir, &self.workspace)
        {
            resolved.session_id = session;
        }
        let identity = &resolved;
        let inner = self.inner.fork_browser();
        if let Some(url) = self
            .links
            .agent_url(&identity.session_id, &identity.agent_id, &self.provider_id)?
            .or_else(|| {
                if identity.agent_id.is_empty() {
                    self.links
                        .get(&identity.session_id, &self.provider_id)
                        .ok()
                        .flatten()
                } else {
                    None
                }
            })
            && self.chat.is_resumable_url(&url)
        {
            *inner.resume_url.lock().await = url;
        }
        Ok(Arc::new(Self {
            harness: self.harness.clone(),
            identity: Some(identity.clone()),
            inner,
            links: Arc::clone(&self.links),
            session: Some(identity.session_id.clone()),
            sessions_dir: self.sessions_dir.clone(),
            workspace: self.workspace.clone(),
            chat: self.chat.clone(),
            provider_id: self.provider_id.clone(),
            stale_link: Arc::new(AtomicBool::new(false)),
        }))
    }

    async fn close(&self) {
        self.inner.shutdown().await;
    }
    async fn set_model_state(&self, toggles: &[config::Toggle]) -> Result<(), String> {
        self.inner.set_model_state(toggles).await
    }
    async fn send(&self, prompt: &str, start_new_chat: bool) -> Result<String, String> {
        let reply = self
            .inner
            .send(prompt, start_new_chat || self.take_stale_link())
            .await?;
        self.try_link().await;
        self.record_agent_message(prompt, &reply);
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
        self.record_agent_message(prompt, &reply);
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
        let chat_url = match (self.session.as_deref(), turn.provider_id.as_deref()) {
            (Some(session), Some(provider)) => self.links.get(session, provider).ok().flatten(),
            _ => None,
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

/// Where the wrapper's own diagnostics go once the harness owns the screen.
fn wrapper_log_path(home: &Path) -> PathBuf {
    home.join("freechatcode").join("freechatcode.log")
}

/// Take the wrapper's own voice off the screen the moment the harness owns it,
/// and give it back when the harness is done.
///
/// The bridge hands the terminal to the harness at 0.00s and keeps writing to
/// its own stdout and stderr: `browser ready in 5.5s` as the page warms,
/// `reply settled after 20 polls` at the end of every turn, and the browser's
/// own notes in between. While the harness renders an alternate-screen TUI
/// there is no cursor to share, so each of those lines is painted wherever the
/// TUI last drew — which is the prompt box the user is typing into. The report
/// was exactly that: a screenshot of `freechatcode: reply settled after 20
/// polls (…)` sitting in Codewhale's prompt box.
///
/// The harness already holds the terminal by the time this runs — its stdio was
/// duplicated at `spawn()` — so repointing the wrapper's own fd 1 and fd 2
/// cannot reach it. From here the wrapper's diagnostics go to a log file and
/// the screen belongs to the harness alone. When the harness exits, the saved
/// descriptors are put back so the wrapper can still report a failed run.
struct ScreenHandover {
    log: PathBuf,
    saved: Vec<(i32, std::fs::File)>,
}

impl ScreenHandover {
    /// Redirect whatever the wrapper writes to a terminal into `log`. Returns
    /// `None` when neither stdout nor stderr is a terminal: nothing is sharing
    /// a screen, and a script that captured those streams still wants the lines
    /// where it put them.
    fn take(log: PathBuf) -> Option<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::OpenOptionsExt;

        let mut targets = Vec::new();
        if std::io::stdout().is_terminal() {
            targets.push(1);
        }
        if std::io::stderr().is_terminal() {
            targets.push(2);
        }
        if targets.is_empty() {
            return None;
        }
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Owner-only: the log carries the same URLs and diagnostics the audit
        // log does.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log)
            .ok()?;
        let mut saved = Vec::new();
        for fd in targets {
            // SAFETY: dup(2)/dup2(2) on the wrapper's own descriptors. `copy`
            // is a fresh duplicate taken before the original is repointed, and
            // it is closed again in `put_back`.
            unsafe {
                let copy = libc::dup(fd);
                if copy < 0 {
                    continue;
                }
                if libc::dup2(file.as_raw_fd(), fd) < 0 {
                    libc::close(copy);
                    continue;
                }
                saved.push((fd, std::fs::File::from_raw_fd(copy)));
            }
        }
        if saved.is_empty() {
            return None;
        }
        Some(Self { log, saved })
    }

    /// Put the wrapper's own stdout/stderr back on the terminal. Idempotent, so
    /// an explicit call and the `Drop` backstop cannot fight.
    fn put_back(&mut self) {
        use std::os::fd::AsRawFd;
        for (fd, saved) in self.saved.drain(..) {
            // SAFETY: restoring the descriptor this struct saved from the same
            // fd, which nothing else repointed in between.
            unsafe {
                libc::dup2(saved.as_raw_fd(), fd);
            }
        }
    }
}

impl Drop for ScreenHandover {
    fn drop(&mut self) {
        self.put_back();
    }
}

/// One-time move of the first release's DeepSeek profile/audit directory into
/// the per-provider layout, so a signed-in browser session is not lost.
///
/// This must run before *any* subcommand that can touch a provider directory.
/// A subcommand that created `providers/<id>/` first made the migration skip
/// itself, which stranded the only signed-in browser profile under the legacy
/// name and left the wrapper looking logged out.
fn migrate_provider_dirs(home: &Path) {
    let legacy = home.join("deepseek-chat");
    let current = home.join("providers").join("deepseek");
    if !legacy.exists() {
        return;
    }
    if !current.exists() {
        if let Some(parent) = current.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::rename(&legacy, &current) {
            eprintln!(
                "freechatcode: could not migrate {} to {}: {error}",
                legacy.display(),
                current.display()
            );
        }
        return;
    }
    // The destination already exists. Move each piece it does not have instead
    // of skipping the whole directory, so an early-created (empty) provider dir
    // cannot strand a signed-in profile or its audit history.
    for name in ["browser", "audit"] {
        let (from, to) = (legacy.join(name), current.join(name));
        if from.exists()
            && !to.exists()
            && let Err(error) = std::fs::rename(&from, &to)
        {
            eprintln!(
                "freechatcode: could not migrate {} to {}: {error}",
                from.display(),
                to.display()
            );
        }
    }
    // `remove_dir` only succeeds once the legacy directory is empty, so this
    // never discards anything the loop above could not move.
    let _ = std::fs::remove_dir(&legacy);
}

/// Check each configured (or the one named) provider: can a browser open, and is
/// the composer present? This is the "set up one provider at a time" path — it
/// never types anything, it only verifies reachability and sign-in.
async fn run_doctor(config: &Config, home: &Path, filter: Option<&str>) -> Result<()> {
    let providers: Vec<&Provider> = match filter {
        Some(id) => match config.provider(id) {
            Some(provider) => vec![provider],
            None => bail!(
                "no provider named {id}; configured: {}",
                config
                    .providers
                    .iter()
                    .map(|provider| provider.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        None => config.providers.iter().collect(),
    };
    let cdp_endpoint = config.browser.cdp_endpoint.clone();
    let mut failures = 0;
    for provider in providers {
        println!("Checking provider {} ({})…", provider.name, provider.id);
        let browser = make_browser(
            config,
            provider,
            home,
            config.browser.profile_dir.clone().map(PathBuf::from),
            cdp_endpoint.clone(),
            provider.chat.url.clone(),
        );
        match browser.ensure_open().await {
            Ok(()) => {
                println!("  OK — browser opened and the composer is present.");
                browser.shutdown().await;
            }
            Err(error) => {
                failures += 1;
                let failure = browser.classify(&error).await;
                println!(
                    "  FAILED [{} / {}]: {}",
                    failure.kind,
                    failure.blame,
                    first_line(&error)
                );
            }
        }
    }
    if failures > 0 {
        bail!("{failures} provider check(s) failed");
    }
    Ok(())
}

async fn configure_provider(home: &Path, config: &mut Config, name: &str) -> Result<()> {
    let id = freechatcode::providers::canonical(name);
    let mut provider = config
        .provider(&id)
        .cloned()
        .or_else(|| {
            freechatcode::providers::catalog()
                .into_iter()
                .find(|p| p.id == id)
        })
        .context("unknown provider; use chatmodels list")?;
    if config.browser.cdp_endpoint.is_some() {
        provider.chat.cdp_endpoint = config.browser.cdp_endpoint.clone();
    }
    let mut visible = config.clone();
    visible.browser.headless = false;
    if visible.browser.cdp_endpoint.is_none() {
        visible.browser.mode = config::BrowserMode::Managed;
    }
    let browser = make_browser(
        &visible,
        &provider,
        home,
        None,
        visible.browser.cdp_endpoint.clone(),
        provider.chat.url.clone(),
    );
    if browser.spec.cdp_endpoint.is_some() {
        println!(
            "Configuring {} using the attached browser's existing sign-in.",
            provider.name
        );
    } else {
        println!(
            "Configuring {}. Sign in in Chromium; the profile is saved per provider.",
            provider.name
        );
    }
    let result = browser.ensure_open().await;
    browser.shutdown().await;
    result.map_err(anyhow::Error::msg)?;
    if let Some(existing) = config
        .providers
        .iter_mut()
        .find(|existing| existing.id == id)
    {
        existing.chat.cdp_endpoint = provider.chat.cdp_endpoint;
    } else {
        config.providers.push(provider);
    }
    if config.default_provider.is_none() {
        config.default_provider = config.providers.first().map(|p| p.id.clone());
    }
    config::save_providers(&config::user_config_path(home), config).map_err(anyhow::Error::msg)?;
    println!(
        "Configured {id}; composer verified. Use doctor and a real turn to verify reply selectors."
    );
    Ok(())
}

async fn run_chatmodels(
    home: &Path,
    command: ChatModelCommands,
    cdp_endpoint: Option<String>,
) -> Result<()> {
    let path = config::user_config_path(home);
    let mut config = Config::load(Some(&path)).map_err(anyhow::Error::msg)?;
    if cdp_endpoint.is_some() {
        config.browser.cdp_endpoint = cdp_endpoint;
    }
    let reconfigure = matches!(&command, ChatModelCommands::ReConfigure { .. });
    match command {
        ChatModelCommands::List => {
            println!("=== All chat models and providers ===\n=configured=");
            for p in &config.providers {
                let default = config
                    .default_model_id()
                    .and_then(|m| config.model(m))
                    .is_some_and(|(provider, _)| provider.id == p.id);
                println!(
                    "{}{}: {}",
                    p.name,
                    if default { " *default" } else { "" },
                    p.models
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if config.default_all {
                println!("All configured is default");
            }
            println!("=not-configured=");
            for p in freechatcode::providers::catalog()
                .iter()
                .filter(|p| config.provider(&p.id).is_none())
            {
                println!(
                    "{}{}",
                    p.name,
                    if p.id == "google-ai-mode" {
                        " (experimental template)"
                    } else {
                        ""
                    }
                );
            }
            println!("Configure with: freechatcode chatmodels configure <provider>");
        }
        ChatModelCommands::Configure { target } => {
            configure_provider(home, &mut config, &target.name()?).await?
        }
        ChatModelCommands::SetDefault { target } => {
            freechatcode::providers::select_default(&mut config, &target.name()?)
                .map_err(anyhow::Error::msg)?;
            config::save_providers(&path, &config).map_err(anyhow::Error::msg)?;
        }
        ChatModelCommands::Drop { target } | ChatModelCommands::ReConfigure { target } => {
            let name = target.name()?;
            let id = freechatcode::providers::drop_provider(&mut config, &name)
                .map_err(anyhow::Error::msg)?;
            config::save_providers(&path, &config).map_err(anyhow::Error::msg)?;
            let profile = home.join("providers").join(&id).join("browser");
            if browser::profile_holder(&profile).is_some() {
                bail!(
                    "configuration dropped; close Chromium before removing its signed-in profile at {}",
                    profile.display()
                );
            }
            if profile.exists() {
                tokio::fs::remove_dir_all(&profile)
                    .await
                    .context("remove provider sign-in profile")?;
            }
            if reconfigure {
                configure_provider(home, &mut config, &id).await?;
            }
        }
    }
    Ok(())
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

async fn run() -> Result<()> {
    let mut forwarded = false;
    let arguments = std::env::args_os().map(|argument| {
        if argument == "--" {
            forwarded = true;
        }
        if !forwarded && argument == "-opt=all" {
            std::ffi::OsString::from("--opt=all")
        } else {
            argument
        }
    });
    let args = Args::parse_from(arguments);
    // Resolve the home and run the one-time directory migration *before* any
    // subcommand can create a provider directory of its own.
    let home = default_home()?;
    migrate_provider_dirs(&home);

    let (binary_hint, codewhale_args) = match args.command {
        Some(Commands::Chatmodels { command }) => {
            run_chatmodels(&home, command, args.cdp_endpoint.clone()).await?;
            return Ok(());
        }
        Some(Commands::Feedback {
            title,
            body,
            body_file,
        }) => {
            let mut command = Command::new("gh");
            command.args([
                "issue",
                "create",
                "--repo",
                "dixonSolutions/freechatcode",
                "--title",
                &title,
            ]);
            if let Some(path) = body_file {
                command.arg("--body-file").arg(path);
            } else {
                command.args(["--body", body.as_deref().unwrap_or("")]);
            }
            if !command
                .status()
                .await
                .context("run gh; install GitHub CLI and sign in with gh auth login")?
                .success()
            {
                bail!("GitHub could not create the feedback issue");
            }
            return Ok(());
        }
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
            print_turns(&home, limit)?;
            return Ok(());
        }
        Some(Commands::Doctor { provider }) => {
            let mut config = Config::load_with_mode(Some(&config::user_config_path(&home)), None)
                .map_err(|error| anyhow::anyhow!(error))?;
            if args.cdp_endpoint.is_some() {
                config.browser.cdp_endpoint = args.cdp_endpoint.clone();
            }
            run_doctor(&config, &home, provider.as_deref()).await?;
            return Ok(());
        }
        Some(Commands::Tui) => {
            run_lobby(args.codewhale_bin.as_deref()).await?;
            (None, args.codewhale_args.clone())
        }
        Some(Commands::Launch {
            binary,
            codewhale_args,
            default: _,
        }) => (binary, codewhale_args),
        None => (None, args.codewhale_args.clone()),
    };

    let config_path = config::user_config_path(&home);
    let mut config = Config::load_with_mode(Some(&config_path), args.mode)
        .map_err(|error| anyhow::anyhow!(error))?;
    if args.cdp_endpoint.is_some() {
        config.browser.cdp_endpoint = args.cdp_endpoint.clone();
    }
    if let Some(name) = &args.chatmodel {
        let id = freechatcode::providers::canonical(name);
        if config.provider(&id).is_none() {
            configure_provider(&home, &mut config, &id).await?;
        }
        if args.make_default {
            freechatcode::providers::select_default(&mut config, &id)
                .map_err(anyhow::Error::msg)?;
            config::save_providers(&config_path, &config).map_err(anyhow::Error::msg)?;
        }
    } else if args.make_default {
        bail!("--make-default requires --chatmodel");
    }
    // CLI flags beat the config file for the two recording knobs.
    if let Some(dir) = args.record_video.clone() {
        config.browser.record_video_dir = Some(dir.to_string_lossy().into_owned());
    }
    if let Some(size) = args.record_video_size.clone() {
        config.browser.record_video_size = Some(size);
    }

    // Resolve the model and its provider for this run. One tab per run for now:
    // `--model` picks a model (and therefore its provider), otherwise the first
    // model of the first provider is used. All of that provider's models are
    // served; multi-provider multi-tab is a follow-up.
    let model_id = args
        .model
        .clone()
        .or_else(|| {
            args.chatmodel
                .as_ref()
                .and_then(|name| config.provider(&freechatcode::providers::canonical(name)))
                .and_then(|p| p.models.first())
                .map(|m| m.id.clone())
        })
        .or_else(|| config.default_model_id().map(str::to_owned))
        .context("no provider or model is configured")?;
    let (active_provider, _) = config.model(&model_id).ok_or_else(|| {
        anyhow::anyhow!(
            "--model must name a configured model (got {model_id}); configured: {}",
            config
                .all_models()
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    let provider = active_provider.clone();
    let expose_all = args.opt.is_some()
        || (args.chatmodel.is_none() && args.model.is_none() && config.default_all);
    println!(
        "Launching with {}: {}",
        if expose_all {
            "all configured models"
        } else {
            "selected model"
        },
        model_id
    );

    // Effective values: CLI flag > environment (merged by clap) > config > default.
    let chat_url = args
        .chat_url
        .clone()
        .unwrap_or_else(|| provider.chat.url.clone());
    if !provider.chat.accepts_url(&chat_url) {
        bail!(
            "--chat-url must be HTTPS on one of: {}",
            provider.chat.allowed_hosts.join(", ")
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
    println!("Using harness binary: {}", codewhale_bin.display());
    let harness_kind = args
        .harness
        .unwrap_or_else(|| HarnessKind::detect(&codewhale_bin));
    if harness_kind == HarnessKind::Openclaw {
        let help = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new(&codewhale_bin).arg("--help").output(),
        )
        .await
        .context("OpenClaw help timed out")?
        .context("inspect OpenClaw command support")?;
        if !help.status.success()
            || !String::from_utf8_lossy(&help.stdout)
                .lines()
                .any(|line| line.split_whitespace().next() == Some("agent"))
        {
            bail!(
                "this OpenClaw executable has no agent command. The Cargo openclaw-cli 0.1.0 package exposes gateway/configuration commands, not a coding harness. Use official OpenClaw with --harness-bin /path/to/openclaw"
            );
        }
    }
    if pinned.is_none()
        && let Err(error) = config::remember_binary(&config_path, &codewhale_bin)
    {
        eprintln!("freechatcode: could not remember the codewhale binary: {error}");
    }

    let cdp_endpoint = args
        .cdp_endpoint
        .clone()
        .or_else(|| config.browser.cdp_endpoint.clone());
    // One audit log for the whole relay, however many providers are running.
    let audit = JsonlAudit::create(&home.join("freechatcode").join("audit")).await?;

    // Resolve the Codewhale session this run will use (shared across providers).
    let workspace = std::env::current_dir().context("resolve the working directory")?;
    let sessions_dir = home.join("sessions");
    let links = Arc::new(
        SessionLinks::open(&home.join("freechatcode").join("sessions.db"))
            .map_err(|error| anyhow::anyhow!(error))?,
    );
    let session = if harness_kind != HarnessKind::Codewhale {
        None
    } else {
        match codewhale_intent(&codewhale_args) {
            Intent::Fresh => None,
            Intent::Session(hint) => {
                sessions::resolve_session_id(&sessions_dir, &workspace, Some(&hint))
            }
            Intent::Auto => sessions::resolve_session_id(&sessions_dir, &workspace, None),
        }
    };

    // Take the terminal off the wrapper's stdin *before* any browser exists, so
    // the driver library has nothing to snapshot and nothing to write back onto
    // the TUI's line discipline. See `take_terminal_off_stdin`.
    let terminal = take_terminal_off_stdin();
    let startup = std::time::Instant::now();
    let (failed_tx, mut failed_rx) =
        tokio::sync::mpsc::channel::<String>(config.providers.len().max(1));

    // One tab (and one conversation) per provider, all served by this one relay.
    let profile_override = args
        .profile_dir
        .clone()
        .or_else(|| config.browser.profile_dir.clone().map(PathBuf::from));
    let mut route_groups: Vec<RouteGroup> = Vec::new();
    let mut browsers: Vec<Arc<BrowserChat>> = Vec::new();
    for entry in config
        .providers
        .iter()
        .filter(|p| expose_all || p.id == provider.id)
    {
        // `--chat-url` overrides the active provider's base URL; every other
        // provider opens at its configured URL.
        let base_url = if entry.id == provider.id {
            chat_url.clone()
        } else {
            entry.chat.url.clone()
        };
        // A stored link is trusted only while it is still resumable on this
        // provider's host; anything else is treated as no link at all.
        let linked_url = session
            .as_ref()
            .and_then(|id| {
                links
                    .agent_url(id, "", &entry.id)
                    .ok()
                    .flatten()
                    .or_else(|| links.get(id, &entry.id).ok().flatten())
            })
            .filter(|url| entry.chat.is_resumable_url(url));
        let had_link = linked_url.is_some();
        if let Some(url) = &linked_url {
            println!(
                "Resuming the {} conversation linked to this Codewhale session: {url}",
                entry.name
            );
        } else if session.is_some() {
            println!(
                "No linked {} conversation yet; opening a new one.",
                entry.name
            );
        }
        let open_url = linked_url.unwrap_or(base_url);

        let browser = Arc::new(make_browser(
            &config,
            entry,
            &home,
            if entry.id == provider.id {
                profile_override.clone()
            } else {
                None
            },
            cdp_endpoint.clone(),
            open_url,
        ));
        browsers.push(Arc::clone(&browser));
        let stale_link = Arc::new(AtomicBool::new(false));
        {
            let browser = Arc::clone(&browser);
            let stale_link = Arc::clone(&stale_link);
            let links = Arc::clone(&links);
            let session = session.clone();
            let notifications = config.relay.desktop_notifications;
            let failed_tx = failed_tx.clone();
            let provider_name = entry.name.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                if let Err(error) = browser.ensure_open().await {
                    // "It never started" is exactly the question a log answers.
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
                    let _ = failed_tx
                        .send(format!(
                            "could not open the {provider_name} browser [{kind} / {blame}]: {condensed}{more}"
                        ))
                        .await;
                    return;
                }
                eprintln!(
                    "freechatcode: {provider_name} browser ready in {:.1}s",
                    started.elapsed().as_secs_f64()
                );
                // The Codewhale session is the source of truth: verify the linked
                // conversation still exists before trusting it.
                if had_link && !browser.link_available().await {
                    stale_link.store(true, Ordering::SeqCst);
                    notify(
                        notifications,
                        &format!("{provider_name} chat link is stale"),
                        "The linked conversation is no longer reachable. Opening a new chat and re-feeding the Codewhale session.",
                    );
                }
            });
        }

        // Keep the browser alive between turns: a browser that is closed or
        // crashes should come back on its own, not at the next prompt.
        if config.browser.keep_alive && config.browser.liveness_check_secs > 0 {
            let watcher = Arc::clone(&browser);
            let interval = Duration::from_secs(config.browser.liveness_check_secs);
            tokio::spawn(async move { watcher.watch_browser(interval).await });
        }

        let ui = Arc::new(UrlLinkingChat {
            harness: format!(
                "{:?}",
                args.harness
                    .unwrap_or_else(|| HarnessKind::detect(&codewhale_bin))
            )
            .to_ascii_lowercase(),
            identity: None,
            inner: Arc::clone(&browser),
            links: Arc::clone(&links),
            session: session.clone(),
            sessions_dir: sessions_dir.clone(),
            workspace: workspace.clone(),
            chat: entry.chat.clone(),
            provider_id: entry.id.clone(),
            stale_link: Arc::clone(&stale_link),
        });
        let models: Vec<ModelSpec> = entry
            .models
            .iter()
            .filter(|model| expose_all || model.id == model_id)
            .map(|model| ModelSpec {
                id: model.id.clone(),
                owned_by: entry.id.clone(),
                name: model.name.clone(),
                toggles: model.toggles.clone(),
            })
            .collect();
        route_groups.push(RouteGroup {
            ui,
            start_fresh: !had_link,
            models,
        });
    }

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
    // The harness's own messages are the prompt: the wrapper writes none of its
    // own, so there is no template to read and no `{project_dir}` to fill.
    let app = router(
        ServerState::with_routes(
            token.clone(),
            route_groups,
            audit,
            BridgeOptions {
                tools: tool_policy,
                // Per-tab freshness lives on each RouteGroup; this field only
                // drives the single-tab convenience constructor.
                start_fresh: false,
            },
        )
        .with_turns(turns)
        .with_default_conversation(freechatcode::ConversationIdentity {
            session_id: session
                .clone()
                .unwrap_or_else(|| format!("run-{}", Uuid::new_v4())),
            agent_id: String::new(),
        }),
    );
    let server = tokio::spawn(async move { serve(listener, app).await });
    let base_url = format!("http://{address}/v1");

    let harness_name = match harness_kind {
        HarnessKind::Codewhale => "Codewhale",
        HarnessKind::Opencode => "opencode",
        HarnessKind::Openclaw => "OpenClaw",
    };
    println!("Starting {harness_name} with the chat browser route.");
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

    // The harness is about to own the terminal, and from the next line on the
    // user will not see the wrapper's own diagnostics on it. Say where they
    // will be. See `ScreenHandover`.
    let log_path = wrapper_log_path(&home);
    if std::io::stdout().is_terminal() {
        println!(
            "freechatcode: the screen goes to the harness; this run's logs: {}",
            log_path.display()
        );
    }

    // The harness still gets the terminal on stdin; the wrapper just no longer
    // holds it itself.
    let child_stdin = match terminal {
        Some(terminal) => Stdio::from(terminal),
        None => Stdio::inherit(),
    };
    let served_models: Vec<String> = config
        .all_models()
        .into_iter()
        .filter(|(id, _)| expose_all || id == &model_id)
        .map(|(id, _)| id)
        .collect();
    let spawn = harness::spawn_contract_with_models(
        harness_kind,
        &base_url,
        &token,
        &model_id,
        &served_models,
    )?;
    let mut command = Command::new(&codewhale_bin);
    command.env("PWD", &workspace);
    command.args(&spawn.argv);
    for (key, value) in &spawn.envs {
        command.env(key, value);
    }
    let mut child = command
        .args(codewhale_args)
        .stdin(child_stdin)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "launch {:?} harness executable {:?}",
                harness_kind, codewhale_bin
            )
        })?;
    // The harness's stdio was duplicated above, so it keeps the terminal. The
    // wrapper gives up its own now, so nothing it writes can land on the TUI.
    let mut handover = ScreenHandover::take(log_path.clone());
    eprintln!(
        "freechatcode: {harness_name} started at {:.2}s (browser warming in parallel)",
        startup.elapsed().as_secs_f64()
    );

    let status = tokio::select! {
        status = child.wait() => status.context("wait for Codewhale")?,
        failure = async {
            match failed_rx.recv().await {
                // Only an actual message is a failure. Senders dropped after a
                // successful warm-up must not read as one, so this branch simply
                // never resolves in that case.
                Some(message) => message,
                None => std::future::pending::<String>().await,
            }
        } => {
            // Codewhale is already up, but nothing can be answered without a
            // browser, so stop here with the same diagnosis the serial version
            // produced — just delivered as soon as it was known.
            child.kill().await.context("stop Codewhale")?;
            child.wait().await.context("reap Codewhale")?;
            server.abort();
            for browser in &browsers {
                browser.shutdown().await;
            }
            bail!("{failure}");
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait for Ctrl+C")?;
            child.kill().await.context("stop Codewhale")?;
            child.wait().await.context("reap Codewhale")?
        }
    };
    server.abort();
    for browser in &browsers {
        browser.shutdown().await;
    }
    // The harness has released the screen. Put the wrapper's own streams back
    // so its last words — including a failed-exit report — land on the
    // terminal, not in the log file.
    if let Some(handover) = handover.as_mut() {
        handover.put_back();
        println!(
            "freechatcode: the harness exited; this run's logs are in {}",
            handover.log.display()
        );
    }
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
    use super::browser::*;
    use super::*;
    use freechatcode::config::{BrowserMode, Selectors, Toggle};
    use playwright_rs::Playwright;
    use playwright_rs::protocol::{BrowserContextOptions, Page};

    fn live_state(
        token: &str,
        ui: Arc<dyn ChatUi>,
        audit: Arc<dyn AuditSink>,
        options: BridgeOptions,
    ) -> ServerState {
        let provider = Config::defaults().providers.remove(0);
        ServerState::with_routes(
            token.to_owned(),
            vec![RouteGroup {
                ui,
                start_fresh: options.start_fresh,
                models: provider
                    .models
                    .into_iter()
                    .map(|model| ModelSpec {
                        id: model.id,
                        owned_by: provider.id.clone(),
                        name: model.name,
                        toggles: model.toggles,
                    })
                    .collect(),
            }],
            audit,
            options,
        )
    }

    const TEST_MODEL_ID: &str = "deepseek-chat";
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek profile and network access"]
    async fn live_file_and_image_attachments_are_read_by_the_chat() {
        use base64::Engine;
        let mut config = Config::defaults();
        config.browser.headless = true;
        let (dir, profile) = profile_copy();
        let path = dir.path().join("attachment.txt");
        std::fs::write(&path, "ATTACHMENT-CHECK-7391").unwrap();
        let ui = make_browser(
            &config,
            &config.providers[0],
            dir.path(),
            Some(profile),
            None,
            config.providers[0].chat.url.clone(),
        );
        let image = base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../tests/fixtures/attachment.png"));
        let prompt = serde_json::json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"Read the attached text file and image. Reply with only the two tokens inside those attachments, one per line."},
            {"type":"file","file":{"path":path,"filename":"attachment.txt","mime_type":"text/plain"}},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{image}")}}
        ]}]}).to_string();
        let reply = ui.send(&prompt, true).await;
        ui.shutdown().await;
        let reply = reply.unwrap();
        assert!(reply.contains("ATTACHMENT-CHECK-7391"), "{reply}");
        assert!(reply.contains("IMAGE-CHECK-8532"), "{reply}");
    }

    #[tokio::test]
    #[ignore = "needs network access to the Gemini web app"]
    async fn live_gemini_browser_round_trip() {
        let mut config = Config::defaults();
        config.browser.headless = true;
        config.timeouts.response_secs = 90;
        let provider = freechatcode::providers::catalog()
            .into_iter()
            .find(|p| p.id == "gemini")
            .unwrap();
        let home = tempfile::tempdir().unwrap();
        let ui = make_browser(
            &config,
            &provider,
            home.path(),
            None,
            None,
            provider.chat.url.clone(),
        );
        ui.ensure_open().await.unwrap();
        let reply = ui
            .send(
                "Reply with exactly GEMINI-BRIDGE-CHECK and nothing else.",
                true,
            )
            .await;
        ui.shutdown().await;
        assert_eq!(reply.unwrap(), "GEMINI-BRIDGE-CHECK");
    }
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek profile and network access"]
    async fn live_independent_agent_tabs_answer_in_parallel() {
        let mut config = Config::defaults();
        config.browser.headless = true;
        let (dir, profile) = profile_copy();
        let parent = Arc::new(make_browser(
            &config,
            &config.providers[0],
            dir.path(),
            Some(profile),
            None,
            config.providers[0].chat.url.clone(),
        ));
        let a = parent.fork_browser();
        let b = parent.fork_browser();
        let (one, two) = tokio::join!(
            a.send("Reply with exactly LIVE-AGENT-ONE.", true),
            b.send("Reply with exactly LIVE-AGENT-TWO.", true)
        );
        let urls = (a.live_url().await, b.live_url().await);
        a.shutdown().await;
        b.shutdown().await;
        parent.shutdown().await;
        assert_eq!(one.unwrap(), "LIVE-AGENT-ONE");
        assert_eq!(two.unwrap(), "LIVE-AGENT-TWO");
        assert!(urls.0.is_some() && urls.1.is_some());
        assert_ne!(urls.0, urls.1);
    }
    #[tokio::test]
    async fn independent_agent_tabs_keep_the_parent_alive() {
        use base64::Engine;
        let playwright = Playwright::launch().await.unwrap();
        let browser = playwright.chromium().launch().await.unwrap();
        let context = browser.new_context().await.unwrap();
        let page = context.new_page().await.unwrap();
        let html = r#"<textarea></textarea><button type=submit onclick="const t=document.querySelector('textarea');const d=document.createElement('div');d.className='ds-markdown';d.textContent=t.value;t.value='';document.body.append(d)">Send</button>"#;
        let mut config = Config::defaults();
        config.providers[0].chat.url = format!(
            "data:text/html;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(html)
        );
        config.timeouts.poll_ms = 10;
        config.timeouts.settle_polls = 2;
        let home = tempfile::tempdir().unwrap();
        let parent = Arc::new(make_browser(
            &config,
            &config.providers[0],
            home.path(),
            None,
            None,
            config.providers[0].chat.url.clone(),
        ));
        *parent.session.lock().await = Some(
            Session::new(page, playwright, context.clone(), false)
                .await
                .unwrap(),
        );
        let a = parent.fork_browser();
        let b = parent.fork_browser();
        let (one, two) = tokio::join!(a.send("AGENT-ONE", false), b.send("AGENT-TWO", false));
        assert_eq!(one.unwrap(), "AGENT-ONE");
        assert_eq!(two.unwrap(), "AGENT-TWO");
        assert_eq!(context.pages().len(), 3);
        a.shutdown().await;
        assert_eq!(context.pages().iter().filter(|p| !p.is_closed()).count(), 2);
        assert!(parent.session.lock().await.as_ref().unwrap().is_alive());
        assert_eq!(b.send("STILL-TWO", false).await.unwrap(), "STILL-TWO");
        b.shutdown().await;
        parent.session.lock().await.as_mut().unwrap().managed = true;
        let mut ephemeral = parent.fork_browser();
        Arc::get_mut(&mut ephemeral).unwrap().keep_alive = false;
        assert_eq!(
            ephemeral.send("EPHEMERAL", false).await.unwrap(),
            "EPHEMERAL"
        );
        assert!(ephemeral.session.lock().await.is_none());
        assert!(parent.session.lock().await.is_none());
        let _ = browser.close().await;
        parent.shutdown().await;
    }
    #[tokio::test]
    async fn native_tool_arguments_use_the_selected_original_markdown() {
        let playwright = Playwright::launch().await.unwrap();
        let browser = playwright.chromium().launch().await.unwrap();
        let page = browser.new_page().await.unwrap();
        let source = "**Reading**\n\n<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"write\"><｜｜DSML｜｜ parameter name=\"content\" string=\"true\">def f():\n    return __name__\n</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>";
        page.set_content("<div class=reply>Reading<br><br>&lt;｜｜DSML｜｜ calls&gt;<br>def f():<br>    return <strong>name</strong></div>", None).await.unwrap();
        page.locator(".reply").evaluate::<(), _>("(element, source) => { element.__reactFiber$test = {memoizedProps:{markdown:source}}; }", Some(&source)).await.unwrap();
        let config = Config::defaults();
        let home = tempfile::tempdir().unwrap();
        let ui = make_browser(
            &config,
            &config.providers[0],
            home.path(),
            None,
            None,
            config.providers[0].chat.url.clone(),
        );
        let value = ui.reply_text(&page.locator(".reply")).await.unwrap();
        assert!(value.contains("def f():\n    return __name__\n"), "{value}");
        assert!(value.starts_with("Reading\n\n<｜｜DSML"), "{value}");
        page.locator(".reply")
            .evaluate::<(), _>(
                "element => { delete element.__reactFiber$test; }",
                None::<&()>,
            )
            .await
            .unwrap();
        assert!(ui.reply_text(&page.locator(".reply")).await.is_err());
        browser.close().await.unwrap();
        playwright.shutdown().await.unwrap();
    }

    #[test]
    fn provider_attachment_endpoint_does_not_leak_to_other_providers() {
        let config = Config::defaults();
        let home = tempfile::tempdir().unwrap();
        let mut google = freechatcode::providers::catalog()
            .into_iter()
            .find(|p| p.id == "gemini")
            .unwrap();
        google.chat.cdp_endpoint = Some("http://127.0.0.1:9317".into());
        let attached = make_browser(
            &config,
            &google,
            home.path(),
            None,
            None,
            google.chat.url.clone(),
        );
        assert_eq!(attached.spec.cdp_endpoint, google.chat.cdp_endpoint);
        let managed = make_browser(
            &config,
            &config.providers[0],
            home.path(),
            None,
            None,
            config.providers[0].chat.url.clone(),
        );
        assert_eq!(managed.spec.cdp_endpoint, None);
        let override_endpoint = Some("http://127.0.0.1:9320".into());
        let override_ui = make_browser(
            &config,
            &google,
            home.path(),
            None,
            override_endpoint.clone(),
            google.chat.url.clone(),
        );
        assert_eq!(override_ui.spec.cdp_endpoint, override_endpoint);
    }

    #[tokio::test]
    async fn truncated_composer_context_is_refused_before_send() {
        let playwright = Playwright::launch().await.unwrap();
        let browser = playwright.chromium().launch().await.unwrap();
        let page = browser.new_page().await.unwrap();
        page.set_content("<textarea maxlength=4></textarea><button type=submit onclick=\"this.dataset.sent='yes'\">Send</button>", None).await.unwrap();
        let config = Config::defaults();
        let home = tempfile::tempdir().unwrap();
        let ui = make_browser(
            &config,
            &config.providers[0],
            home.path(),
            None,
            None,
            config.providers[0].chat.url.clone(),
        );
        let error = ui
            .submit(&page, "longer request", false, None)
            .await
            .unwrap_err();
        assert!(error.contains("truncated"), "{error}");
        assert_eq!(
            page.locator("button")
                .get_attribute("data-sent")
                .await
                .unwrap(),
            None
        );
        browser.close().await.unwrap();
        playwright.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn attachments_reach_the_browser_file_input() {
        let playwright = Playwright::launch().await.unwrap();
        let browser = playwright.chromium().launch().await.unwrap();
        let page = browser.new_page().await.unwrap();
        page.set_content("<input type=file multiple style='display:none'>", None)
            .await
            .unwrap();
        let config = Config::defaults();
        let home = tempfile::tempdir().unwrap();
        let ui = make_browser(
            &config,
            &config.providers[0],
            home.path(),
            None,
            None,
            config.providers[0].chat.url.clone(),
        );
        let prompt = serde_json::json!({"messages":[{"content":[{"type":"file","file":{"filename":"note.txt","file_data":"data:text/plain;base64,RklMRS1UT0tFTg=="}}]}]}).to_string();
        ui.upload_attachments(&page, &prompt).await.unwrap();
        let value = page.evaluate::<(), Value>("async () => { const f = document.querySelector('input').files[0]; return {name:f.name,type:f.type,text:await f.text()}; }", None).await.unwrap();
        assert_eq!(
            value,
            serde_json::json!({"name":"note.txt","type":"text/plain","text":"FILE-TOKEN"})
        );
        browser.close().await.unwrap();
        playwright.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_linking_wrapper_delegates_model_state() {
        let playwright = Playwright::launch().await.unwrap();
        let browser = playwright.chromium().launch().await.unwrap();
        let context = browser.new_context().await.unwrap();
        let page = context.new_page().await.unwrap();
        page.set_content("<button class='mode' aria-pressed='false' onclick=\"this.setAttribute('aria-pressed',this.getAttribute('aria-pressed')==='true'?'false':'true')\">Think</button>",None).await.unwrap();
        let config = Config::defaults();
        let home = tempfile::tempdir().unwrap();
        let inner = Arc::new(make_browser(
            &config,
            &config.providers[0],
            home.path(),
            None,
            None,
            config.providers[0].chat.url.clone(),
        ));
        *inner.session.lock().await = Some(
            Session::new(page.clone(), playwright, context, false)
                .await
                .unwrap(),
        );
        let ui = UrlLinkingChat {
            harness: "test".into(),
            identity: None,
            inner,
            links: Arc::new(SessionLinks::open(&home.path().join("sessions.db")).unwrap()),
            session: Some("s".into()),
            sessions_dir: home.path().into(),
            workspace: home.path().into(),
            chat: config.providers[0].chat.clone(),
            provider_id: "deepseek".into(),
            stale_link: Arc::new(AtomicBool::new(false)),
        };
        ui.set_model_state(&[Toggle {
            selector: ".mode".into(),
            on: true,
        }])
        .await
        .unwrap();
        assert_eq!(chip_is_selected(&page, ".mode").await, Some(true));
        ui.set_model_state(&[Toggle {
            selector: ".mode".into(),
            on: false,
        }])
        .await
        .unwrap();
        assert_eq!(chip_is_selected(&page, ".mode").await, Some(false));
        browser.close().await.unwrap();
        ui.inner.shutdown().await;
    }
    const TEST_PRO_MODEL_ID: &str = "deepseek-pro";

    #[test]
    fn chat_url_acceptance_follows_config() {
        let config = Config::defaults();
        assert!(
            config.providers[0]
                .chat
                .accepts_url("https://chat.deepseek.com/")
        );
        assert!(
            !config.providers[0]
                .chat
                .accepts_url("http://chat.deepseek.com/")
        );
        assert!(
            !config.providers[0]
                .chat
                .accepts_url("https://chat.deepseek.com.attacker.invalid/")
        );
        assert!(
            !config.providers[0]
                .chat
                .accepts_url("https://user@chat.deepseek.com/")
        );
        // The old deepseek.ai host is no longer trusted by default.
        assert!(
            !config.providers[0]
                .chat
                .accepts_url("https://deepseek.ai/chat")
        );
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

    /// The managed browser profile the runtime would use for the shipped
    /// provider, derived through `make_browser` rather than written out — so a
    /// test cannot point at a directory the wrapper stopped using.
    fn provider_profile(home: &Path) -> PathBuf {
        let config = Config::load(Some(&config::user_config_path(home))).expect("config");
        let provider = config.providers.first().expect("a configured provider");
        make_browser(
            &config,
            provider,
            home,
            None,
            None,
            provider.chat.url.clone(),
        )
        .spec
        .profile
    }

    #[test]
    fn the_managed_profile_belongs_to_the_provider_under_the_codewhale_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let profile = provider_profile(home.path());

        assert_eq!(
            profile,
            home.path()
                .join("providers")
                .join("deepseek")
                .join("browser"),
            "the profile lives under the provider, in the Codewhale home"
        );
        assert!(
            profile.starts_with(home.path()),
            "the managed browser is never the user's everyday Chromium profile"
        );
    }

    #[test]
    fn migration_still_runs_when_the_provider_directory_already_exists() {
        let home = tempfile::tempdir().expect("tempdir");
        let home = home.path();
        // The first release kept the signed-in profile and its audit log under
        // `deepseek-chat/`.
        let legacy = home.join("deepseek-chat");
        std::fs::create_dir_all(legacy.join("browser").join("Default")).expect("legacy profile");
        std::fs::write(
            legacy.join("browser").join("Default").join("Cookies"),
            b"signed-in",
        )
        .expect("write cookie");
        std::fs::create_dir_all(legacy.join("audit")).expect("legacy audit");
        // A subcommand created the per-provider directory first, and left it
        // empty. This is what used to make the migration skip itself and strand
        // the only signed-in profile under the legacy name.
        std::fs::create_dir_all(home.join("providers").join("deepseek")).expect("provider dir");

        migrate_provider_dirs(home);

        let current = home.join("providers").join("deepseek");
        assert!(
            current
                .join("browser")
                .join("Default")
                .join("Cookies")
                .exists(),
            "a signed-in profile must not be stranded in the legacy directory"
        );
        assert!(
            current.join("audit").exists(),
            "the audit history moves too"
        );
        assert!(!legacy.exists(), "the emptied legacy directory is removed");
    }

    #[test]
    fn migration_never_clobbers_a_profile_already_in_the_new_layout() {
        let home = tempfile::tempdir().expect("tempdir");
        let home = home.path();
        let legacy = home.join("deepseek-chat").join("browser");
        std::fs::create_dir_all(&legacy).expect("legacy profile");
        std::fs::write(legacy.join("legacy-cookie"), b"old").expect("write");
        let current = home.join("providers").join("deepseek").join("browser");
        std::fs::create_dir_all(&current).expect("current profile");
        std::fs::write(current.join("current-cookie"), b"new").expect("write");

        migrate_provider_dirs(home);

        assert!(
            current.join("current-cookie").exists(),
            "the profile in the new layout is left alone"
        );
        assert!(
            legacy.join("legacy-cookie").exists(),
            "and the other one is left in place, never deleted"
        );
    }

    #[test]
    fn local_provider_token_is_ephemeral_and_not_an_account_key() {
        let token = format!("cw_{}", Uuid::new_v4().simple());
        assert_eq!(token.len(), 35);
        assert!(token.starts_with("cw_"));
    }

    #[test]
    fn default_selectors_support_a_test_page_without_platform_private_endpoints() {
        let selectors = Config::defaults().providers[0].selectors.clone();
        assert!(selectors.composer.contains("textarea"));
        assert!(selectors.assistant.contains("ds-markdown"));
        assert!(selectors.send.contains("send"));
        assert!(selectors.new_chat.contains("new-chat"));
        // Verified against DeepSeek's own stylesheet, which styles the reasoning
        // as `.ds-think-content .ds-markdown` — i.e. the reasoning *is* a
        // markdown block, and would be caught by `assistant` without this.
        assert_eq!(selectors.reasoning, ".ds-think-content");
    }

    #[test]
    fn the_pages_reasoning_is_excluded_from_every_alternative_of_the_reply_selector() {
        let selectors = Config::defaults().providers[0].selectors.clone();
        let selector = browser::reply_selector(&selectors);
        assert_eq!(
            selector,
            format!(
                ":is({}):not(:is(.ds-think-content)):not(:is(.ds-think-content) *)",
                selectors.assistant
            )
        );

        // A provider that renders no reasoning gets its selector back untouched.
        let mut plain = selectors.clone();
        plain.reasoning = String::new();
        assert_eq!(browser::reply_selector(&plain), selectors.assistant);
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
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
        let (_profile_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = false;
        let workspace = std::env::current_dir().expect("cwd");
        let ui = Arc::new(BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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

        let state = live_state(
            "live-token",
            ui.clone(),
            Arc::new(QuietAudit),
            BridgeOptions {
                tools: ToolPolicy::default(),
                start_fresh: true,
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
                "model": TEST_MODEL_ID,
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
                "model": TEST_MODEL_ID,
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
        let (_profile_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = false;
        let ui = BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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

        ui.set_model_state(&[Toggle {
            selector: ui.selectors().thinking_toggle.trim().to_owned(),
            on: false,
        }])
        .await
        .expect("the chat model must be reachable");
        let chat = chips(&ui, &page).await;
        eprintln!("[pro] after chat: {chat:?}");
        assert!(
            chat.contains("DeepThink=off") || !chat.contains("DeepThink"),
            "the plain chat model must leave DeepThink off, saw {chat:?}"
        );

        ui.set_model_state(&[Toggle {
            selector: ui.selectors().thinking_toggle.trim().to_owned(),
            on: true,
        }])
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
        ui.set_model_state(&[Toggle {
            selector: ui.selectors().thinking_toggle.trim().to_owned(),
            on: false,
        }])
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
    /// `set_model_state` directly; this one goes through the real router, so the
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
        ui.set_model_state(&[Toggle {
            selector: ui.selectors().thinking_toggle.trim().to_owned(),
            on: false,
        }])
        .await
        .expect("start in chat mode");
        let before = chips(&ui, &page).await;
        eprintln!("[relay] chips before: {before:?}");
        assert!(!before.contains("DeepThink=on"), "{before:?}");

        let audit = JsonlAudit::create(&dir.path().join("audit"))
            .await
            .expect("audit log");
        let state = live_state("secret", ui.clone(), audit, BridgeOptions::default());
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
                "model": TEST_PRO_MODEL_ID,
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
                "model": TEST_MODEL_ID,
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
        let state = live_state("secret", ui.clone(), audit, BridgeOptions::default());
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
                "model": TEST_MODEL_ID,
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
        ui.set_model_state(&[Toggle {
            selector: ui.selectors().thinking_toggle.trim().to_owned(),
            on: false,
        }])
        .await
        .expect("start in chat mode");
        ui.set_model_state(&[Toggle {
            selector: ui.selectors().thinking_toggle.trim().to_owned(),
            on: true,
        }])
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

    /// Live: the model is the harness's coder, not a narrator of how it is reached.
    ///
    /// Reported from a real session: the model answered "I'm reached through the
    /// DeepSeek Chat web page, driven in a browser you're signed in to..." and
    /// recited a session handshake, because the wrapper's instruction text
    /// described the wrapper instead of handing over the harness's own briefing.
    /// No instruction text is written by the wrapper any more: the harness's
    /// messages are the prompt, so the model sees the briefing and nothing else.
    /// This sends a bare message and requires an answer that does not talk about
    /// its own transport, and it checks the audit to prove the briefing was
    /// actually bridged and that nothing was added around it.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_the_model_is_a_coder_not_a_narrator_of_its_transport() {
        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let (dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        let ui = Arc::new(BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
        let state = live_state("secret", ui.clone(), audit, BridgeOptions::default());
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
                "model": TEST_MODEL_ID,
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
        // And nothing else: no wrapper-written preamble, no contract, no reminder.
        for authored in [
            "You are a Codewhale coder",
            "Codewhale request:",
            "Reminder: prose finishes the turn",
            "Here is the answer.",
        ] {
            assert!(
                !prompt_seen.contains(authored),
                "the wrapper wrote its own prompt again ({authored:?}); the prompt must be \
                 the harness's request and nothing else"
            );
        }

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

    /// Live: the reported message comes back as a delivered reply.
    ///
    /// The input is the user's own, verbatim — not a prompt written to elicit a
    /// tool call, which is what made the first version of this test prove so
    /// little. A live model is stochastic, so this asserts the guarantee the relay
    /// actually provides — whatever the page says is delivered, parsed and never
    /// dropped, with no re-ask and no rewriting — and prints what happened.
    #[tokio::test]
    #[ignore = "needs a signed-in DeepSeek Chat profile; run with -- --ignored --nocapture"]
    async fn live_a_chatty_request_does_not_end_the_turn_on_a_promise() {
        install_crypto_provider();
        let home = default_home().expect("codewhale home");
        let (_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        config.browser.headless = true;
        let ui = Arc::new(BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
        let state = live_state(
            "secret",
            ui.clone(),
            Arc::new(QuietAudit),
            BridgeOptions::default(),
        );
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
                "model": TEST_MODEL_ID,
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
        // The invariant the mechanism provides: the page's reply is delivered.
        // Either the model acted (a tool call was parsed out of what it wrote) or
        // it answered in prose, which on this transport *is* the answer. Nothing
        // is dropped, re-asked or rewritten in between.
        let acted = finish == "tool_calls"
            && message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty());
        assert!(
            acted || !content.trim().is_empty(),
            "the turn delivered nothing at all: finish_reason={finish:?} \
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
            profile_dir.join("freechatcode"),
            target.join("release/freechatcode"),
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
            .args([
                "--mode",
                "silent",
                "--harness",
                "codewhale",
                "--codewhale-bin",
            ])
            .arg(setup::resolve_codewhale_binary("codewhale").expect("installed codewhale"))
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
        let source = provider_profile(&home);
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
                profile: provider_profile(&home),
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
            provider_id: None,
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
        config.providers[0].chat.url = url.to_owned();
        BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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

        let source = provider_profile(&home);
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
            config.providers[0].chat.is_resumable_url(&conversation),
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
    /// Set `FREECHATCODE_INSPECT_URL` to the conversation (defaults to the chat
    /// URL). Runs on a copy of the profile, so it never disturbs a live session.
    #[tokio::test]
    #[ignore = "diagnostic: inspects a live page; needs a signed-in profile"]
    async fn live_inspect_page() {
        let home = default_home().expect("codewhale home");
        let config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        let url = std::env::var("FREECHATCODE_INSPECT_URL")
            .unwrap_or_else(|_| config.providers[0].chat.url.clone());
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

        let assistants = page.locator(&config.providers[0].selectors.assistant);
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
        let composer = page
            .locator(&config.providers[0].selectors.composer)
            .first();
        eprintln!(
            "[inspect] composer visible: {} | enabled: {}",
            composer.is_visible().await.unwrap_or(false),
            composer.is_enabled().await.unwrap_or(false)
        );
        // The reply-detection contract is "how many assistant elements are in
        // the DOM?", so print exactly what the page keeps mounted. A chat UI
        // that unmounts off-screen messages makes that count stop growing, and
        // this report is how that is diagnosed instead of guessed at.
        for line in dom_outline(&page, &config.providers[0].selectors).await {
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
            let before = mounted_count(&page, &config.providers[0].selectors).await;
            let started = std::time::Instant::now();
            let reply = ui
                .send(&padded_prompt(&token), turn == 1)
                .await
                .unwrap_or_else(|error| panic!("turn {turn} failed: {error}"));
            let after = mounted_count(&page, &config.providers[0].selectors).await;
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
                for line in dom_outline(&page, &config.providers[0].selectors).await {
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
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: config.providers[0].chat.url.clone(),
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
        let (_profile_dir, profile) = profile_copy();
        let mut config = Config::load(Some(&config::user_config_path(&home))).expect("config");
        // A visible window so a signed-out profile can be signed in by hand.
        config.browser.headless = false;
        // Optional recording, so a demo clip can be produced from a real turn.
        if let Ok(dir) = std::env::var("FREECHATCODE_RECORD_VIDEO_DIR") {
            config.browser.record_video_dir = Some(dir);
        }
        if let Ok(size) = std::env::var("FREECHATCODE_RECORD_VIDEO_SIZE") {
            config.browser.record_video_size = Some(size);
        }
        let prompt = std::env::var("FREECHATCODE_DEMO_PROMPT")
            .unwrap_or_else(|_| "Reply with exactly the word PONG and nothing else.".to_owned());
        let workspace = std::env::current_dir().expect("cwd");
        let sessions_dir = home.join("sessions");
        let links = SessionLinks::open(&home.join("freechatcode").join("sessions.db"))
            .expect("open link store");
        let session = sessions::resolve_session_id(&sessions_dir, &workspace, None);
        let linked = session
            .as_ref()
            .and_then(|id| links.get(id, "deepseek").ok().flatten())
            .filter(|url| config.providers[0].chat.is_resumable_url(url));
        eprintln!("[live] session={session:?}");
        eprintln!("[live] linked conversation={linked:?}");
        let start_fresh = linked.is_none();

        let ui = BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: linked.unwrap_or_else(|| config.providers[0].chat.url.clone()),
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
            config.providers[0].chat.is_resumable_url(&conversation),
            "a real turn must leave a resumable conversation URL: {conversation}"
        );
        ui.shutdown().await;

        let resumed = BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
                url: conversation.clone(),
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
    /// This runs the shipped wrapper on a real pty with no harness arguments,
    /// so the harness runs its interactive TUI, and asserts what the user is
    /// left with once the browser is warm: `ECHO` still clear across consecutive
    /// samples, the wrapper's own fd 0 not a terminal while the child it spawned
    /// holds this terminal on fd 0, a mouse report written to the master not
    /// coming back as text, and — issue #4 — no wrapper diagnostic reaching the
    /// screen after the wrapper hands it over (they go to its log file instead).
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

        /// The byte offset of the last `needle` in `haystack`.
        fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
            if needle.is_empty() || haystack.len() < needle.len() {
                return None;
            }
            (0..=haystack.len() - needle.len())
                .rev()
                .find(|&at| &haystack[at..at + needle.len()] == needle)
        }

        /// The byte offset of the first `needle` in `haystack`.
        fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
            if needle.is_empty() {
                return None;
            }
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        }

        /// The wrapper's log path, read from the handover notice it prints on
        /// the terminal just before it gives the screen to the harness.
        fn log_path_of(output: &[u8]) -> Option<String> {
            let text = String::from_utf8_lossy(output);
            let (_, rest) = text.rsplit_once("this run's logs: ")?;
            let path = rest.lines().next()?.trim();
            (!path.is_empty()).then(|| path.to_owned())
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
                    libc::ioctl(master, libc::TIOCSWINSZ, &window);
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

        // Once the harness owns the screen the wrapper's own diagnostics go to
        // this file instead of the terminal (see `ScreenHandover`), so the
        // warm-up line is read from here. Snapshot its length first: the log is
        // appended to across runs, and a previous run's line must not satisfy
        // the check.
        let expected_log = wrapper_log_path(&default_home().expect("codewhale home"));
        let logged_before = std::fs::metadata(&expected_log)
            .map(|meta| meta.len())
            .unwrap_or(0);

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
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
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
            holds(&output, b"\x1b[?1006h"),
            "Codewhale must be in its interactive TUI (no mouse tracking seen); output: {}",
            tail(&output)
        );

        // Issue #4: the wrapper must stop painting on the screen the moment the
        // harness owns it. Everything after its handover notice belongs to the
        // harness; one wrapper line there is the bug the user screenshotted.
        let notice = b"the screen goes to the harness";
        let at = rfind(&output, notice).expect("the wrapper must announce the screen handover");
        assert_eq!(
            log_path_of(&output).as_deref().map(Path::new),
            Some(expected_log.as_path()),
            "the handover notice must name the log file this run writes to"
        );
        let after = &output[at + notice.len()..];
        // The prefix every wrapper diagnostic carries. The harness's own TUI
        // also prints its cwd, and that path happens to contain
        // `freechatcode` — so require the colon and space the wrapper writes,
        // not the bare name.
        const WRAPPER_LINE: &[u8] = b"freechatcode: ";
        if let Some(leak) = find(after, WRAPPER_LINE) {
            let from = leak.saturating_sub(80);
            let to = (leak + 240).min(after.len());
            panic!(
                "the wrapper must not write to the screen once the harness owns it (#4); \
                 it still wrote: {}",
                String::from_utf8_lossy(&after[from..to])
            );
        }

        // And the line it stopped printing on the screen must still exist: the
        // warm-up is recorded in the log file instead.
        let logged = std::fs::read(&expected_log).unwrap_or_default();
        let fresh = &logged[(logged_before as usize).min(logged.len())..];
        assert!(
            holds(fresh, b"browser ready in"),
            "the warm-up must be recorded in {} once the screen is handed over; it held: {}",
            expected_log.display(),
            tail(fresh)
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

    /// Issue #8 and #9 against a real page shape.
    ///
    /// DeepSeek with DeepThink on renders the model's reasoning above its answer,
    /// as markdown inside a `.ds-think-content` panel — DeepSeek's own stylesheet
    /// says exactly that (`.ds-think-content .ds-markdown { … }`), which is why it
    /// used to match the `assistant` selector and reach the harness as the model's
    /// answer. The page here models that shape, with the reasoning appearing
    /// first and the answer 300 ms later.
    ///
    /// The reply must be the answer text, and the reasoning must not appear in
    /// the reply *or* in a single streamed snapshot.
    #[tokio::test]
    async fn the_pages_reasoning_is_never_the_reply() {
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
                  const main = document.querySelector('main');
                  // The page thinks first: markdown, inside the think panel.
                  const thinking = document.createElement('div');
                  thinking.className = 'ds-think-content';
                  const reasoning = document.createElement('div');
                  reasoning.className = 'ds-markdown';
                  reasoning.textContent =
                    'The user just says "hello, how are you?" This is for asking.';
                  thinking.append(reasoning);
                  main.append(thinking);
                  // Then the answer, which the wrapper must read.
                  setTimeout(() => {
                    const answer = document.createElement('div');
                    answer.className = 'ds-markdown';
                    answer.textContent = 'Hello! I am doing well.';
                    main.append(answer);
                  }, 300);
                });
              </script>"#,
            None,
        )
        .await
        .expect("install local UI fixture");

        let config = Config::defaults();
        let ui = BrowserChat {
            parent: None,
            spec: BrowserSpec {
                config: config.clone(),
                provider: config.providers[0].clone(),
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

        let (snapshots, mut seen) = tokio::sync::mpsc::channel::<String>(16);
        let collected = tokio::spawn(async move {
            let mut all = Vec::new();
            while let Some(snapshot) = seen.recv().await {
                all.push(snapshot);
            }
            all
        });

        let answer = ui
            .send_streaming("fixture prompt", false, snapshots)
            .await
            .expect("visible UI response");
        ui.shutdown().await;
        let snapshots = collected.await.expect("snapshot collector");

        assert_eq!(
            answer, "Hello! I am doing well.",
            "the reply must be the page's answer, never its reasoning"
        );
        for snapshot in &snapshots {
            assert!(
                !snapshot.contains("The user just says"),
                "the page's reasoning reached the stream: {snapshot:?}"
            );
        }
    }
}
