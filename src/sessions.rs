//! Codewhale session discovery and the Codewhale-session → DeepSeek-chat link
//! store.
//!
//! Codewhale keeps one JSON file per session at
//! `<CODEWHALE_HOME>/sessions/<uuid>.json`, carrying `metadata.id`,
//! `metadata.workspace` and `metadata.updated_at`. The wrapper uses that to
//! find the session that a run will resume, then maps it to the DeepSeek Chat
//! conversation URL it relays through.
//!
//! Codewhale's OpenAI-compatible requests carry no session identifier (verified
//! against a live loopback capture: headers are content-type, accept,
//! authorization, user-agent, host, content-length only), so this mapping has
//! to be driven from the launcher side.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::Value;

/// SQLite-backed map from a Codewhale session id to the DeepSeek Chat
/// conversation URL that session relays through.
pub struct SessionLinks {
    conn: Mutex<Connection>,
}

impl SessionLinks {
    /// Open (creating if needed) the link database at `path`.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
            restrict_directory(parent)?;
        }
        let conn = Connection::open(path)
            .map_err(|error| format!("open session database {}: {error}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("restrict session database: {e}"))?;
        }
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS agent_links (
                 session_id TEXT NOT NULL, agent_id TEXT NOT NULL, provider_id TEXT NOT NULL,
                 chat_url TEXT NOT NULL, updated_at INTEGER NOT NULL,
                 PRIMARY KEY (session_id, agent_id, provider_id)
             );
             CREATE TABLE IF NOT EXISTS agent_messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
                 agent_id TEXT NOT NULL, provider_id TEXT NOT NULL, chat_url TEXT,
                 prompt_bytes INTEGER NOT NULL, reply_bytes INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );
             -- Two bridges (or a reader) can touch this file at once; wait rather
             -- than failing a turn over a lock.
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS chat_links (
                 codewhale_session_id TEXT NOT NULL,
                 provider_id          TEXT NOT NULL,
                 chat_url             TEXT NOT NULL,
                 created_at           INTEGER NOT NULL,
                 updated_at           INTEGER NOT NULL,
                 PRIMARY KEY (codewhale_session_id, provider_id)
             );
             CREATE TABLE IF NOT EXISTS chat_turns (
                 id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                 codewhale_session_id TEXT,
                 chat_url             TEXT,
                 model_label          TEXT,
                 finish_reason        TEXT,
                 tool_calls           INTEGER NOT NULL DEFAULT 0,
                 content_chars        INTEGER NOT NULL DEFAULT 0,
                 created_at           INTEGER NOT NULL,
                 outcome              TEXT NOT NULL DEFAULT 'answered',
                 failure_kind         TEXT,
                 blame                TEXT,
                 http_status          INTEGER,
                 detail               TEXT
             );",
        )
        .map_err(|error| format!("initialise session database: {error}"))?;
        migrate_links(&conn).map_err(|error| format!("migrate link table: {error}"))?;
        migrate_turns(&conn).map_err(|error| format!("migrate session database: {error}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The linked conversation URL for `session_id` on `provider_id`, if any.
    pub fn get(&self, session_id: &str, provider_id: &str) -> Result<Option<String>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "session database lock poisoned".to_owned())?;
        let mut statement = conn
            .prepare(
                "SELECT chat_url FROM chat_links
                 WHERE codewhale_session_id = ?1 AND provider_id = ?2",
            )
            .map_err(|error| format!("prepare link lookup: {error}"))?;
        let mut rows = statement
            .query([session_id, provider_id])
            .map_err(|error| format!("query link: {error}"))?;
        match rows.next().map_err(|error| format!("read link: {error}"))? {
            Some(row) => Ok(Some(
                row.get(0)
                    .map_err(|error| format!("decode link: {error}"))?,
            )),
            None => Ok(None),
        }
    }

    pub fn agent_url(
        &self,
        session: &str,
        agent: &str,
        provider: &str,
    ) -> Result<Option<String>, String> {
        use rusqlite::OptionalExtension;
        self.conn.lock().map_err(|_| "session database lock poisoned")?
            .query_row("SELECT chat_url FROM agent_links WHERE session_id=?1 AND agent_id=?2 AND provider_id=?3", rusqlite::params![session,agent,provider], |r| r.get(0)).optional().map_err(|e| e.to_string())
    }

    pub fn link_agent(
        &self,
        session: &str,
        agent: &str,
        provider: &str,
        url: &str,
    ) -> Result<(), String> {
        self.conn.lock().map_err(|_| "session database lock poisoned")?
            .execute("INSERT INTO agent_links VALUES (?1,?2,?3,?4,?5) ON CONFLICT(session_id,agent_id,provider_id) DO UPDATE SET chat_url=excluded.chat_url, updated_at=excluded.updated_at", rusqlite::params![session,agent,provider,url,unix_seconds()]).map(|_| ()).map_err(|e| e.to_string())
    }

    pub fn record_agent_message(
        &self,
        session: &str,
        agent: &str,
        provider: &str,
        prompt_bytes: usize,
        reply_bytes: usize,
    ) -> Result<(), String> {
        let url = self.agent_url(session, agent, provider)?;
        self.conn.lock().map_err(|_|"session database lock poisoned")?.execute("INSERT INTO agent_messages (session_id,agent_id,provider_id,chat_url,prompt_bytes,reply_bytes,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",rusqlite::params![session,agent,provider,url,prompt_bytes as i64,reply_bytes as i64,unix_seconds()]).map(|_|()).map_err(|e|e.to_string())
    }

    /// Insert or update the conversation URL for `session_id` on `provider_id`.
    pub fn upsert(&self, session_id: &str, provider_id: &str, url: &str) -> Result<(), String> {
        let now = unix_seconds();
        let conn = self
            .conn
            .lock()
            .map_err(|_| "session database lock poisoned".to_owned())?;
        conn.execute(
            "INSERT INTO chat_links
                 (codewhale_session_id, provider_id, chat_url, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(codewhale_session_id, provider_id) DO UPDATE SET
                 chat_url  = excluded.chat_url,
                 updated_at = excluded.updated_at",
            rusqlite::params![session_id, provider_id, url, now],
        )
        .map_err(|error| format!("record link: {error}"))?;
        Ok(())
    }

    /// Number of stored links (used by tests).
    #[cfg(test)]
    fn count(&self) -> Result<i64, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "session database lock poisoned".to_owned())?;
        conn.query_row("SELECT COUNT(*) FROM chat_links", [], |row| row.get(0))
            .map_err(|error| format!("count links: {error}"))
    }

    /// Append one finished turn — answered or failed — so a conversation can be
    /// attributed model-by-model after the fact, and a failure is a record rather
    /// than a silence.
    pub fn record_turn(&self, turn: &TurnRow) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "session database lock poisoned".to_owned())?;
        conn.execute(
            "INSERT INTO chat_turns
                 (codewhale_session_id, chat_url, model_label,
                  finish_reason, tool_calls, content_chars, created_at,
                  outcome, failure_kind, blame, http_status, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                turn.session_id,
                turn.chat_url,
                turn.model_label,
                turn.finish_reason,
                i64::from(turn.tool_calls),
                turn.content_chars as i64,
                unix_seconds(),
                turn.outcome,
                turn.failure_kind,
                turn.blame,
                turn.http_status.map(i64::from),
                turn.detail,
            ],
        )
        .map_err(|error| format!("record turn: {error}"))?;
        Ok(())
    }

    /// The most recent turns, newest first.
    pub fn turns(&self, limit: usize) -> Result<Vec<TurnRow>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "session database lock poisoned".to_owned())?;
        let mut statement = conn
            .prepare(
                "SELECT codewhale_session_id, chat_url, model_label,
                        finish_reason, tool_calls, content_chars, created_at,
                        outcome, failure_kind, blame, http_status, detail
                 FROM chat_turns ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|error| format!("prepare turn query: {error}"))?;
        let rows = statement
            .query_map([limit as i64], |row| {
                Ok(TurnRow {
                    session_id: row.get(0)?,
                    chat_url: row.get(1)?,
                    model_label: row.get(2)?,
                    finish_reason: row.get(3)?,
                    tool_calls: row.get::<_, i64>(4)? != 0,
                    content_chars: row.get::<_, i64>(5)?.max(0) as usize,
                    created_at: row.get(6)?,
                    outcome: row.get(7)?,
                    failure_kind: row.get(8)?,
                    blame: row.get(9)?,
                    http_status: row.get::<_, Option<i64>>(10)?.map(|s| s as u16),
                    detail: row.get(11)?,
                })
            })
            .map_err(|error| format!("query turns: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read turn: {error}"))
    }
}

/// Rebuild `chat_links` from the single-provider schema to the per-provider one.
fn migrate_links(conn: &Connection) -> Result<(), rusqlite::Error> {
    if table_columns(conn, "chat_links")?.contains("provider_id") {
        return Ok(());
    }
    // First release: `codewhale_session_id` as a single-column key and a
    // DeepSeek-named URL column. Rebuild with a (session, provider) key; the
    // only provider that ever existed was DeepSeek.
    conn.execute_batch(
        "CREATE TABLE chat_links_new (
             codewhale_session_id TEXT NOT NULL,
             provider_id          TEXT NOT NULL,
             chat_url             TEXT NOT NULL,
             created_at           INTEGER NOT NULL,
             updated_at           INTEGER NOT NULL,
             PRIMARY KEY (codewhale_session_id, provider_id)
         );
         INSERT INTO chat_links_new
             (codewhale_session_id, provider_id, chat_url, created_at, updated_at)
         SELECT codewhale_session_id, 'deepseek', deepseek_chat_url, created_at, updated_at
         FROM chat_links;
         DROP TABLE chat_links;
         ALTER TABLE chat_links_new RENAME TO chat_links;",
    )?;
    Ok(())
}

/// Add any column an older database is missing, and rename the first release's
/// DeepSeek-named URL column to the provider-neutral `chat_url`.
fn migrate_turns(conn: &Connection) -> Result<(), rusqlite::Error> {
    let existing = table_columns(conn, "chat_turns")?;
    if existing.contains("deepseek_chat_url") && !existing.contains("chat_url") {
        conn.execute(
            "ALTER TABLE chat_turns RENAME COLUMN deepseek_chat_url TO chat_url",
            [],
        )?;
    }
    let existing = table_columns(conn, "chat_turns")?;
    let wanted = [
        ("outcome", "TEXT NOT NULL DEFAULT 'answered'"),
        ("failure_kind", "TEXT"),
        ("blame", "TEXT"),
        ("http_status", "INTEGER"),
        ("detail", "TEXT"),
    ];
    for (column, definition) in wanted {
        if !existing.contains(column) {
            conn.execute(
                &format!("ALTER TABLE chat_turns ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn table_columns(
    conn: &Connection,
    table: &str,
) -> Result<std::collections::BTreeSet<String>, rusqlite::Error> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    names.collect::<Result<_, _>>()
}

/// One row of the turn log: which model answered — or did not — in which
/// conversation, and whose fault it was when nothing came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRow {
    pub session_id: Option<String>,
    pub chat_url: Option<String>,
    pub model_label: Option<String>,
    pub finish_reason: Option<String>,
    pub tool_calls: bool,
    pub content_chars: usize,
    pub created_at: i64,
    /// `answered` or `failed`.
    pub outcome: String,
    /// `dns`, `network`, `page_silent`, `browser_gone`, `request`, … when failed.
    pub failure_kind: Option<String>,
    /// `network`, `wrapper`, `service`, or `unknown` when failed.
    pub blame: Option<String>,
    pub http_status: Option<u16>,
    pub detail: Option<String>,
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
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

/// Metadata read from one Codewhale session file.
struct SessionMeta {
    id: String,
    workspace: String,
    updated_at: String,
}

fn session_entries(sessions_dir: &Path) -> Vec<SessionMeta> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(metadata) = value.get("metadata") else {
            continue;
        };
        let (Some(id), Some(workspace)) = (
            metadata.get("id").and_then(Value::as_str),
            metadata.get("workspace").and_then(Value::as_str),
        ) else {
            continue;
        };
        found.push(SessionMeta {
            id: id.to_owned(),
            workspace: workspace.to_owned(),
            updated_at: metadata
                .get("updated_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    found
}

/// The most recently updated Codewhale session id whose workspace matches
/// `workspace`, as recorded in `<sessions_dir>/<uuid>.json`.
#[must_use]
pub fn active_session_id(sessions_dir: &Path, workspace: &Path) -> Option<String> {
    let workspace = workspace.to_string_lossy();
    session_entries(sessions_dir)
        .into_iter()
        .filter(|meta| meta.workspace == workspace.as_ref())
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .map(|meta| meta.id)
}

/// Resolve the session a run will use. With `hint` (an explicit `-r`/`--resume`
/// value) an exact id or unique id prefix wins; otherwise the newest session
/// for `workspace` is used.
#[must_use]
pub fn resolve_session_id(
    sessions_dir: &Path,
    workspace: &Path,
    hint: Option<&str>,
) -> Option<String> {
    let Some(hint) = hint else {
        return active_session_id(sessions_dir, workspace);
    };
    let ids: Vec<String> = session_entries(sessions_dir)
        .into_iter()
        .map(|meta| meta.id)
        .collect();
    if ids.iter().any(|id| id == hint) {
        return Some(hint.to_owned());
    }
    let matches: Vec<&String> = ids.iter().filter(|id| id.starts_with(hint)).collect();
    (matches.len() == 1).then(|| matches[0].clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn agents_in_different_sessions_keep_independent_links_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("links.db");
        let links = super::SessionLinks::open(&path).unwrap();
        links.link_agent("s1", "a", "p", "https://chat/a").unwrap();
        links.link_agent("s2", "a", "p", "https://chat/b").unwrap();
        links
            .link_agent("s1", "", "p", "https://chat/main")
            .unwrap();
        drop(links);
        let links = super::SessionLinks::open(&path).unwrap();
        assert_eq!(
            links.agent_url("s1", "a", "p").unwrap().as_deref(),
            Some("https://chat/a")
        );
        assert_eq!(
            links.agent_url("s2", "a", "p").unwrap().as_deref(),
            Some("https://chat/b")
        );
        assert_eq!(
            links.agent_url("s1", "", "p").unwrap().as_deref(),
            Some("https://chat/main")
        );
    }
    use super::*;

    fn write_session(dir: &Path, id: &str, workspace: &str, updated_at: &str) {
        let body = serde_json::json!({
            "schema_version": 1,
            "metadata": {
                "id": id,
                "workspace": workspace,
                "updated_at": updated_at,
            }
        });
        std::fs::write(dir.join(format!("{id}.json")), body.to_string()).expect("write session");
    }

    #[test]
    fn picks_newest_session_for_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path();
        write_session(sessions, "old", "/work", "2026-01-01T00:00:00Z");
        write_session(sessions, "new", "/work", "2026-06-01T00:00:00Z");
        write_session(sessions, "other", "/elsewhere", "2026-12-01T00:00:00Z");
        // A non-session file that shares the directory must be ignored.
        std::fs::write(sessions.join("session_boot_owners.json"), "{}").expect("write noise");

        assert_eq!(
            active_session_id(sessions, Path::new("/work")).as_deref(),
            Some("new")
        );
    }

    #[test]
    fn returns_none_without_a_matching_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_session(dir.path(), "s", "/work", "2026-01-01T00:00:00Z");
        assert_eq!(active_session_id(dir.path(), Path::new("/other")), None);
    }

    #[test]
    fn missing_sessions_directory_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            active_session_id(&dir.path().join("absent"), Path::new("/work")),
            None
        );
    }

    #[test]
    fn explicit_hint_accepts_exact_id_and_unique_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path();
        write_session(sessions, "aaaa1111-2222", "/work", "2026-01-01T00:00:00Z");
        write_session(sessions, "bbbb3333-4444", "/work", "2026-02-01T00:00:00Z");

        assert_eq!(
            resolve_session_id(sessions, Path::new("/work"), Some("aaaa1111-2222")).as_deref(),
            Some("aaaa1111-2222")
        );
        assert_eq!(
            resolve_session_id(sessions, Path::new("/work"), Some("aaaa")).as_deref(),
            Some("aaaa1111-2222")
        );
        assert_eq!(
            resolve_session_id(sessions, Path::new("/work"), Some("zzzz")),
            None
        );
        assert_eq!(
            resolve_session_id(sessions, Path::new("/work"), None).as_deref(),
            Some("bbbb3333-4444")
        );
    }

    fn answered_turn(session: &str, finish: &str, chars: usize) -> TurnRow {
        TurnRow {
            session_id: Some(session.to_owned()),
            chat_url: Some("https://chat.deepseek.com/a/chat/s/one".to_owned()),
            model_label: Some("DeepSeek-V4".to_owned()),
            finish_reason: Some(finish.to_owned()),
            tool_calls: finish == "tool_calls",
            content_chars: chars,
            created_at: 0,
            outcome: "answered".to_owned(),
            failure_kind: None,
            blame: None,
            http_status: None,
            detail: None,
        }
    }

    #[test]
    fn turn_log_round_trips_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionLinks::open(&dir.path().join("freechatcode/sessions.db")).expect("open");
        assert!(store.turns(10).expect("empty").is_empty());

        store
            .record_turn(&answered_turn("session-a", "tool_calls", 0))
            .expect("record first");
        store
            .record_turn(&answered_turn("session-a", "stop", 42))
            .expect("record second");

        let turns = store.turns(10).expect("turns");
        assert_eq!(turns.len(), 2);
        // Newest first.
        assert_eq!(turns[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(turns[0].content_chars, 42);
        assert!(!turns[0].tool_calls);
        assert_eq!(turns[0].outcome, "answered");
        assert_eq!(turns[1].model_label.as_deref(), Some("DeepSeek-V4"));
        assert!(turns[1].tool_calls);
        assert_eq!(turns[1].session_id.as_deref(), Some("session-a"));
        assert_eq!(store.turns(1).expect("limited").len(), 1);
    }

    #[test]
    fn a_failed_turn_is_recorded_with_its_diagnosis() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionLinks::open(&dir.path().join("freechatcode/sessions.db")).expect("open");
        // No session at all — the record must still exist.
        store
            .record_turn(&TurnRow {
                session_id: None,
                chat_url: None,
                model_label: Some("DeepThink=off, Search=on".to_owned()),
                finish_reason: Some("error".to_owned()),
                tool_calls: false,
                content_chars: 0,
                created_at: 0,
                outcome: "failed".to_owned(),
                failure_kind: Some("dns".to_owned()),
                blame: Some("network".to_owned()),
                http_status: None,
                detail: Some("net::ERR_NAME_NOT_RESOLVED".to_owned()),
            })
            .expect("record failure");

        let turns = store.turns(1).expect("turns");
        assert_eq!(turns[0].outcome, "failed");
        assert_eq!(turns[0].failure_kind.as_deref(), Some("dns"));
        assert_eq!(turns[0].blame.as_deref(), Some("network"));
        assert_eq!(turns[0].session_id, None);
        assert_eq!(
            turns[0].detail.as_deref(),
            Some("net::ERR_NAME_NOT_RESOLVED")
        );
    }

    #[test]
    fn an_older_database_gains_the_new_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old.db");
        {
            // Exactly the first-release schema.
            let conn = Connection::open(&path).expect("open raw");
            conn.execute_batch(
                "CREATE TABLE chat_turns (
                     id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                     codewhale_session_id TEXT,
                     deepseek_chat_url    TEXT,
                     model_label          TEXT,
                     finish_reason        TEXT,
                     tool_calls           INTEGER NOT NULL DEFAULT 0,
                     content_chars        INTEGER NOT NULL DEFAULT 0,
                     created_at           INTEGER NOT NULL
                 );
                 INSERT INTO chat_turns
                     (codewhale_session_id, model_label, finish_reason, tool_calls,
                      content_chars, created_at)
                 VALUES ('old-session', 'DeepSeek-V4', 'stop', 0, 12, 1);",
            )
            .expect("seed old schema");
        }

        let store = SessionLinks::open(&path).expect("open and migrate");
        let turns = store.turns(10).expect("turns");
        assert_eq!(turns.len(), 1, "the old row must survive the migration");
        // An old row has no failure, and reads as answered.
        assert_eq!(turns[0].outcome, "answered");
        assert_eq!(turns[0].failure_kind, None);
        assert_eq!(turns[0].content_chars, 12);
        // And the table now accepts the new fields.
        store
            .record_turn(&TurnRow {
                session_id: Some("new".to_owned()),
                chat_url: None,
                model_label: None,
                finish_reason: Some("error".to_owned()),
                tool_calls: false,
                content_chars: 0,
                created_at: 0,
                outcome: "failed".to_owned(),
                failure_kind: Some("upstream_http".to_owned()),
                blame: Some("service".to_owned()),
                http_status: Some(404),
                detail: Some("HTTP 404 from the chat endpoint".to_owned()),
            })
            .expect("record into the migrated table");
        let turns = store.turns(1).expect("turns");
        assert_eq!(turns[0].http_status, Some(404));
        assert_eq!(turns[0].blame.as_deref(), Some("service"));
    }

    #[test]
    fn link_store_round_trips_and_updates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionLinks::open(&dir.path().join("freechatcode/sessions.db")).expect("open");

        assert_eq!(store.get("session-a", "deepseek").expect("get"), None);
        store
            .upsert(
                "session-a",
                "deepseek",
                "https://chat.deepseek.com/a/chat/s/one",
            )
            .expect("upsert");
        assert_eq!(
            store.get("session-a", "deepseek").expect("get").as_deref(),
            Some("https://chat.deepseek.com/a/chat/s/one")
        );

        store
            .upsert(
                "session-a",
                "deepseek",
                "https://chat.deepseek.com/a/chat/s/two",
            )
            .expect("re-upsert");
        assert_eq!(
            store.get("session-a", "deepseek").expect("get").as_deref(),
            Some("https://chat.deepseek.com/a/chat/s/two")
        );
        // A different provider is a different link, not an overwrite.
        store
            .upsert("session-a", "gemini", "https://gemini.google.com/app/x")
            .expect("gemini upsert");
        assert_eq!(
            store.get("session-a", "gemini").expect("get").as_deref(),
            Some("https://gemini.google.com/app/x")
        );
        assert_eq!(store.count().expect("count"), 2);
    }
}
