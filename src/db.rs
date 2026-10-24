use std::path::PathBuf;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// Your repos, threads you take part in, and tracked tasks. Watched-only repos (`subscribed`) stay out.
pub const RELEVANT: &str = "(done = 0 AND (mine = 1
     OR reason IN ('author', 'comment', 'mention', 'team_mention', 'review_requested', 'assign', 'manual', 'state_change')
     OR key IN (SELECT key FROM authored)))
     OR t.id IN (SELECT id FROM tasks)";

#[derive(Clone, Debug)]
pub struct Thread {
    pub id: String,
    pub repo: String,
    pub kind: String,
    pub title: String,
    pub url: String,
    pub reason: String,
    pub updated: String,
    pub unread: bool,
    pub mine: bool,
    pub state: Option<String>,
    pub done: bool,
    /// Tracked task stage (0 or 1); the final stage is derived from `state`.
    pub stage: Option<u8>,
    /// Linear-style: 0 none, 1 urgent, 2 high, 3 medium, 4 low.
    pub priority: u8,
    /// Latest event, e.g. "@bob: looks good" or "PR merged".
    pub event: Option<String>,
}

pub struct Upsert<'a> {
    pub id: &'a str,
    pub key: Option<String>,
    pub repo: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub url: String,
    pub reason: &'a str,
    pub updated: &'a str,
    pub unread: bool,
    pub mine: bool,
}

pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/Application Support/ossdeck")
}

/// `OSSDECK_DEMO=<db path>` runs on a mock database with syncing off (used for screenshots).
pub fn demo() -> Option<PathBuf> {
    std::env::var_os("OSSDECK_DEMO").map(PathBuf::from)
}

pub fn open() -> Result<Connection> {
    std::fs::create_dir_all(data_dir())?;
    let c = Connection::open(demo().unwrap_or_else(|| data_dir().join("ossdeck.db")))?;
    c.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS threads (
             id TEXT PRIMARY KEY,
             key TEXT,
             repo TEXT NOT NULL,
             kind TEXT NOT NULL,
             title TEXT NOT NULL,
             url TEXT NOT NULL,
             reason TEXT NOT NULL,
             updated TEXT NOT NULL,
             unread INTEGER NOT NULL,
             mine INTEGER NOT NULL,
             done INTEGER NOT NULL DEFAULT 0,
             state TEXT,
             state_at TEXT,
             event TEXT
         );
         CREATE INDEX IF NOT EXISTS threads_updated ON threads (updated DESC);
         CREATE TABLE IF NOT EXISTS authored (key TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS tasks (
             id TEXT PRIMARY KEY,
             stage INTEGER NOT NULL DEFAULT 0,
             priority INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);",
    )?;
    Ok(c)
}

/// A JSON string list stored in kv, empty when missing.
pub fn get_list(c: &Connection, k: &str) -> Result<Vec<String>> {
    Ok(get(c, k)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Owners whose repos count as "mine": you, plus orgs you opted in.
pub fn mine_owners(c: &Connection) -> Result<Vec<String>> {
    let mut owners = get_list(c, "mine_orgs")?;
    owners.extend(get(c, "me")?);
    Ok(owners.into_iter().map(|o| o.to_lowercase()).collect())
}

pub fn recompute_mine(c: &Connection) -> Result<()> {
    c.execute(
        "UPDATE threads SET mine = lower(substr(repo, 1, instr(repo, '/') - 1)) IN (SELECT value FROM json_each(?))",
        [serde_json::to_string(&mine_owners(c)?)?],
    )?;
    Ok(())
}

pub fn get(c: &Connection, k: &str) -> Result<Option<String>> {
    Ok(
        c.query_row("SELECT v FROM kv WHERE k = ?", [k], |r| r.get(0))
            .optional()?,
    )
}

pub fn set(c: &Connection, k: &str, v: &str) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO kv (k, v) VALUES (?, ?)", [k, v])?;
    Ok(())
}

/// Returns true when the thread is new or has activity newer than what we stored.
pub fn upsert(c: &Connection, t: &Upsert) -> Result<bool> {
    let prev: Option<String> = c
        .query_row("SELECT updated FROM threads WHERE id = ?", [t.id], |r| {
            r.get(0)
        })
        .optional()?;
    c.execute(
        "INSERT INTO threads (id, key, repo, kind, title, url, reason, updated, unread, mine)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (id) DO UPDATE SET
             key = excluded.key, repo = excluded.repo, kind = excluded.kind,
             title = excluded.title, url = excluded.url, reason = excluded.reason,
             unread = excluded.unread, mine = excluded.mine,
             done = CASE WHEN excluded.updated > threads.updated THEN 0 ELSE threads.done END,
             updated = excluded.updated",
        params![
            t.id, t.key, t.repo, t.kind, t.title, t.url, t.reason, t.updated, t.unread, t.mine
        ],
    )?;
    Ok(prev.is_none_or(|p| p.as_str() < t.updated))
}

pub fn load(c: &Connection, ids: Option<&[String]>) -> Result<Vec<Thread>> {
    let filter = if ids.is_some() {
        "AND t.id IN (SELECT value FROM json_each(?1))"
    } else {
        ""
    };
    let sql = format!(
        "SELECT t.id, repo, kind, title, url, reason, updated, unread, mine, state, done, k.stage, COALESCE(k.priority, 0), event
         FROM threads t LEFT JOIN tasks k ON k.id = t.id
         WHERE ({RELEVANT}) {filter} ORDER BY updated DESC LIMIT 1000"
    );
    let mut stmt = c.prepare(&sql)?;
    let row = |r: &rusqlite::Row| {
        Ok(Thread {
            id: r.get(0)?,
            repo: r.get(1)?,
            kind: r.get(2)?,
            title: r.get(3)?,
            url: r.get(4)?,
            reason: r.get(5)?,
            updated: r.get(6)?,
            unread: r.get(7)?,
            mine: r.get(8)?,
            state: r.get(9)?,
            done: r.get(10)?,
            stage: r.get(11)?,
            priority: r.get(12)?,
            event: r.get(13)?,
        })
    };
    let rows: rusqlite::Result<Vec<_>> = match ids {
        Some(ids) => stmt
            .query_map([serde_json::to_string(ids)?], row)?
            .collect(),
        None => stmt.query_map([], row)?.collect(),
    };
    Ok(rows?)
}
