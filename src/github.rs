use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{db, time};

const API: &str = "https://api.github.com/";
const MAX_RETRIES: usize = 3;
/// Wait for a 429 that carries no rate limit headers; GitHub's docs say "at least one minute".
const MIN_WAIT_SECS: u64 = 60;
/// Longer rate limit waits fail the sync instead; the next poll tries again.
const MAX_WAIT_SECS: u64 = 120;
const MAX_PAGES: u32 = 100;
/// Concurrent page requests; GitHub penalises bursts of parallel calls.
const PARALLEL: usize = 8;
/// GitHub keeps unsaved notifications for 5 months, so import everything it still has.
const HISTORY_DAYS: i64 = 150;

#[derive(Deserialize)]
struct Note {
    id: String,
    unread: bool,
    reason: String,
    updated_at: String,
    subject: Subject,
    repository: Repo,
}

#[derive(Deserialize)]
struct Subject {
    title: String,
    url: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Repo {
    full_name: String,
    private: bool,
    html_url: String,
    owner: Owner,
}

#[derive(Deserialize)]
struct Owner {
    login: String,
}

struct Resp {
    status: u16,
    link: Option<String>,
    last_modified: Option<String>,
    poll_interval: Option<String>,
    body: Value,
}

pub struct Github {
    agent: ureq::Agent,
    auth: String,
    pub me: String,
}

impl Github {
    pub fn new() -> Result<Self> {
        let token = match std::env::var("GITHUB_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => gh_token()?,
        };
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        let mut gh = Self {
            agent,
            auth: format!("Bearer {token}"),
            me: String::new(),
        };
        gh.me = gh.get("user", None)?.body["login"]
            .as_str()
            .context("no login")?
            .to_owned();
        Ok(gh)
    }

    fn get(&self, path: &str, if_modified: Option<&str>) -> Result<Resp> {
        let mut r = retry(|| {
            let mut req = self
                .agent
                .get(format!("{API}{path}"))
                .header("Authorization", &self.auth)
                .header("Accept", "application/vnd.github+json");
            if let Some(lm) = if_modified {
                req = req.header("If-Modified-Since", lm);
            }
            req.call()
        })?;
        let status = r.status().as_u16();
        let header = |n: &str| {
            r.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (link, last_modified, poll_interval) = (
            header("link"),
            header("last-modified"),
            header("x-poll-interval"),
        );
        let body = match status {
            304 => Value::Null,
            200..300 => r.body_mut().read_json()?,
            _ => bail!(
                "GET {path}: {status} {}",
                r.body_mut().read_to_string().unwrap_or_default()
            ),
        };
        Ok(Resp {
            status,
            link,
            last_modified,
            poll_interval,
            body,
        })
    }

    fn graphql(&self, query: &str) -> Result<Value> {
        let body = json!({ "query": query });
        let mut r = retry(|| {
            self.agent
                .post(format!("{API}graphql"))
                .header("Authorization", &self.auth)
                .send_json(&body)
        })?;
        ensure!(r.status().is_success(), "graphql: {}", r.status());
        Ok(r.body_mut().read_json()?)
    }

    pub fn mark_read(&self, id: &str) -> Result<()> {
        let r = retry(|| {
            self.agent
                .patch(format!("{API}notifications/threads/{id}"))
                .header("Authorization", &self.auth)
                .send_empty()
        })?;
        ensure!(r.status().is_success(), "mark read: {}", r.status());
        Ok(())
    }

    pub fn mark_done(&self, id: &str) -> Result<()> {
        let r = retry(|| {
            self.agent
                .delete(format!("{API}notifications/threads/{id}"))
                .header("Authorization", &self.auth)
                .call()
        })?;
        ensure!(r.status().is_success(), "mark done: {}", r.status());
        Ok(())
    }

    /// Fetches every page; pages after the first are fetched in parallel.
    fn notifications(
        &self,
        c: &Connection,
        query: &str,
        if_modified: Option<&str>,
    ) -> Result<Option<(Vec<Note>, Option<String>)>> {
        let base = format!("notifications?{query}&per_page=50");
        let first = self.get(&base, if_modified)?;
        if let Some(p) = &first.poll_interval {
            db::set(c, "poll_interval", p)?;
        }
        if first.status == 304 {
            return Ok(None);
        }
        let last = first
            .link
            .as_deref()
            .and_then(last_page)
            .unwrap_or(1)
            .min(MAX_PAGES);
        let pages: Vec<u32> = (2..=last).collect();
        let mut rest = Vec::new();
        for wave in pages.chunks(PARALLEL) {
            rest.extend(std::thread::scope(|s| {
                let handles: Vec<_> = wave
                    .iter()
                    .map(|p| {
                        let url = format!("{base}&page={p}");
                        s.spawn(move || self.get(&url, None).map(|r| r.body))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("page thread"))
                    .collect::<Result<Vec<_>>>()
            })?);
        }
        let mut notes: Vec<Note> = serde_json::from_value(first.body)?;
        for page in rest {
            notes.extend(serde_json::from_value::<Vec<Note>>(page)?);
        }
        Ok(Some((notes, first.last_modified)))
    }

    /// Orgs where you can admin or maintain public repos; you pick which count as "mine".
    fn refresh_admin_orgs(&self, c: &Connection) -> Result<()> {
        let mut orgs = std::collections::BTreeSet::new();
        let mut path = Some(
            "user/repos?affiliation=organization_member,collaborator&visibility=public&per_page=100".to_owned(),
        );
        while let Some(p) = path.take() {
            let r = self.get(&p, None)?;
            for repo in r.body.as_array().into_iter().flatten() {
                let perms = &repo["permissions"];
                if (perms["admin"] == true || perms["maintain"] == true)
                    && let Some(owner) = repo["owner"]["login"].as_str()
                    && !owner.eq_ignore_ascii_case(&self.me)
                {
                    orgs.insert(owner.to_owned());
                }
            }
            path = r.link.as_deref().and_then(next_page);
        }
        db::set(c, "admin_orgs", &serde_json::to_string(&orgs)?)
    }

    fn refresh_authored(&self, c: &Connection) -> Result<()> {
        let since = &time::iso(time::now() - HISTORY_DAYS * 86_400)[..10];
        let q = format!("author:{0} is:public -user:{0} updated:>={since}", self.me);
        let r = self.get(
            &format!("search/issues?per_page=100&sort=updated&q={}", encode(&q)),
            None,
        )?;
        let tx = c.unchecked_transaction()?;
        tx.execute("DELETE FROM authored", [])?;
        for key in r.body["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| thread_key(i["url"].as_str()?))
        {
            tx.execute("INSERT OR IGNORE INTO authored (key) VALUES (?)", [key])?;
        }
        Ok(tx.commit()?)
    }

    fn refresh_states(&self, c: &Connection, all: bool) -> Result<()> {
        let stale = if all {
            ""
        } else {
            "AND (state_at IS NULL OR state_at < updated OR event IS NULL)"
        };
        let todo: Vec<(String, String)> = c
            .prepare(&format!(
                "SELECT t.id, key FROM threads t WHERE key IS NOT NULL
                 AND kind IN ('PullRequest', 'Issue') {stale} AND ({})",
                db::RELEVANT
            ))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for chunk in todo.chunks(50) {
            let mut q = String::from("query{");
            for (i, (_, key)) in chunk.iter().enumerate() {
                let Some(((owner, name), num)) = key
                    .split_once('#')
                    .and_then(|(r, n)| Some((r.split_once('/')?, n)))
                else {
                    continue;
                };
                write!(
                    q,
                    "t{i}:repository(owner:{},name:{}){{issueOrPullRequest(number:{num}){{__typename \
                     ...on PullRequest{{state isDraft {LAST} reviews(last:1){{nodes{{{NODE}}}}}}} \
                     ...on Issue{{state stateReason {LAST}}}}}}}",
                    json!(owner),
                    json!(name)
                )?;
            }
            q.push('}');
            let data = self.graphql(&q)?;
            let tx = c.unchecked_transaction()?;
            for (i, (id, _)) in chunk.iter().enumerate() {
                let node = &data["data"][format!("t{i}")]["issueOrPullRequest"];
                tx.execute(
                    "UPDATE threads SET state = ?, event = ?, state_at = updated WHERE id = ?",
                    (state_of(node), last_comment(node, &self.me), id),
                )?;
            }
            tx.commit()?;
        }
        Ok(())
    }
}

type Response = ureq::http::Response<ureq::Body>;

/// Retry policy ported from octocrab's `RetryConfig::HandleRateLimits`, plus its default
/// retry on network errors:
/// - 403/429 with rate limit headers: wait for Retry-After or until X-RateLimit-Reset (min 5s).
/// - 429 without them: wait MIN_WAIT_SECS. 403 without them is a real "forbidden": no retry.
/// - 5xx and network errors: retry immediately.
fn retry(send: impl Fn() -> Result<Response, ureq::Error>) -> Result<Response> {
    let mut left = MAX_RETRIES;
    loop {
        let res = send();
        let wait = match &res {
            Err(_) => Some(0),
            Ok(r) if r.status().is_server_error() => Some(0),
            Ok(r) if matches!(r.status().as_u16(), 403 | 429) => rate_limit_wait(r),
            Ok(_) => None,
        };
        match wait {
            Some(secs) if left > 0 && secs <= MAX_WAIT_SECS => {
                left -= 1;
                std::thread::sleep(Duration::from_secs(secs));
            }
            _ => return Ok(res?),
        }
    }
}

fn rate_limit_wait(r: &Response) -> Option<u64> {
    let header = |n: &str| r.headers().get(n)?.to_str().ok()?.parse::<i64>().ok();
    match (
        header("retry-after"),
        header("x-ratelimit-remaining"),
        header("x-ratelimit-reset"),
    ) {
        (Some(secs), _, _) => Some(secs.max(0) as u64),
        (None, Some(0), Some(reset)) => Some((reset - time::now()).max(5) as u64),
        _ if r.status() == 429 => Some(MIN_WAIT_SECS),
        _ => None,
    }
}

/// One sync pass. Returns relevant threads with new unread activity (empty on the first run).
pub fn sync(gh: &Github, c: &Connection, force: bool) -> Result<Vec<db::Thread>> {
    let now = time::now();
    let older = |k: &str, secs: i64| -> Result<bool> {
        Ok(db::get(c, k)?
            .and_then(|v| v.parse::<i64>().ok())
            .is_none_or(|t| now - t > secs))
    };
    // History is re-imported when HISTORY_DAYS changes.
    let seed = format!("{HISTORY_DAYS}d");
    let seeded = db::get(c, "seeded")?.as_deref() == Some(seed.as_str());
    db::set(c, "me", &gh.me)?;
    if force || !seeded || older("orgs_at", 3600)? {
        gh.refresh_admin_orgs(c)?;
        db::set(c, "orgs_at", &now.to_string())?;
    }
    if !seeded {
        let since = time::iso(now - HISTORY_DAYS * 86_400);
        if let Some((notes, _)) = gh.notifications(c, &format!("all=true&since={since}"), None)? {
            store(c, &notes)?;
        }
    }
    let mut fresh = Vec::new();
    if force || !seeded || older("authored_at", 600)? {
        gh.refresh_authored(c)?;
        db::set(c, "authored_at", &now.to_string())?;
    }

    // Conditional requests (304) are free, but a read made elsewhere may not bump
    // Last-Modified, so do an unconditional pass every few minutes.
    let full = force || older("full_at", 300)?;
    let lm = if full { None } else { db::get(c, "unread_lm")? };
    let unread = gh.notifications(c, "all=false", lm.as_deref())?;
    if let Some((notes, last_modified)) = &unread {
        fresh = store(c, notes)?.iter().map(|n| n.id.clone()).collect();
        let ids: Vec<&str> = notes.iter().map(|n| n.id.as_str()).collect();
        c.execute(
            "UPDATE threads SET unread = 0 WHERE unread = 1 AND id NOT IN (SELECT value FROM json_each(?))",
            [serde_json::to_string(&ids)?],
        )?;
        // New activity from others on an upstream thread moves its task to stage 1.
        c.execute(
            "UPDATE tasks SET stage = 1 WHERE stage = 0 AND id IN
             (SELECT id FROM threads WHERE mine = 0 AND id IN (SELECT value FROM json_each(?)))",
            [serde_json::to_string(&fresh)?],
        )?;
        if let Some(lm) = last_modified {
            db::set(c, "unread_lm", lm)?;
        }
        if full {
            db::set(c, "full_at", &now.to_string())?;
        }
    }
    let all_states = older("states_at", 3600)?;
    gh.refresh_states(c, all_states)?;
    if all_states {
        db::set(c, "states_at", &now.to_string())?;
    }
    db::set(c, "seeded", &seed)?;
    if !seeded {
        return Ok(Vec::new());
    }
    Ok(db::load(c, Some(&fresh))?
        .into_iter()
        .filter(|t| t.unread && !t.done)
        .collect())
}

/// Notification text: the state change, or else the latest comment.
pub fn describe(t: &db::Thread) -> String {
    let kind = kind_label(&t.kind);
    match (t.reason.as_str(), t.state.as_deref()) {
        ("state_change", Some("open" | "draft")) => format!("{kind} reopened"),
        ("state_change", Some("not_planned")) => format!("{kind} closed as not planned"),
        ("state_change", Some(state)) => format!("{kind} {state}"),
        (reason, _) => t
            .event
            .clone()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| format!("{kind} - {}", reason.replace('_', " "))),
    }
}

const NODE: &str = "author{login} bodyText createdAt";
const LAST: &str = "comments(last:1){nodes{author{login} bodyText createdAt}}";

/// Latest comment or review as "@who: text"; empty when there is none.
fn last_comment(node: &Value, me: &str) -> String {
    let latest = [&node["comments"]["nodes"][0], &node["reviews"]["nodes"][0]]
        .into_iter()
        .filter(|c| !c.is_null())
        .max_by_key(|c| c["createdAt"].as_str().unwrap_or_default().to_owned());
    let Some(c) = latest else {
        return String::new();
    };
    let who = match c["author"]["login"].as_str() {
        Some(l) if l.eq_ignore_ascii_case(me) => "you".to_owned(),
        Some(l) => format!("@{l}"),
        None => "someone".to_owned(),
    };
    let text = c["bodyText"]
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text: String = text.chars().take(600).collect();
    if text.is_empty() {
        format!("{who} reviewed")
    } else {
        format!("{who}: {text}")
    }
}

/// Stores public notifications; returns those that are new or updated and unread.
fn store<'a>(c: &Connection, notes: &'a [Note]) -> Result<Vec<&'a Note>> {
    let owners = db::mine_owners(c)?;
    let tx = c.unchecked_transaction()?;
    let mut fresh = Vec::new();
    for n in notes.iter().filter(|n| !n.repository.private) {
        let changed = db::upsert(
            &tx,
            &db::Upsert {
                id: &n.id,
                key: n.subject.url.as_deref().and_then(thread_key),
                repo: &n.repository.full_name,
                kind: &n.subject.kind,
                title: &n.subject.title,
                url: html_url(n),
                reason: &n.reason,
                updated: &n.updated_at,
                unread: n.unread,
                mine: owners.contains(&n.repository.owner.login.to_lowercase()),
            },
        )?;
        if changed && n.unread {
            fresh.push(n);
        }
    }
    tx.commit()?;
    Ok(fresh)
}

pub fn kind_label(kind: &str) -> &str {
    match kind {
        "PullRequest" => "PR",
        "CheckSuite" => "CI",
        "RepositoryVulnerabilityAlert" | "RepositoryDependabotAlertsThread" => "Security alert",
        k => k,
    }
}

fn state_of(n: &Value) -> Option<&'static str> {
    Some(match (n["__typename"].as_str()?, n["state"].as_str()?) {
        ("PullRequest", "MERGED") => "merged",
        ("PullRequest", "CLOSED") => "closed",
        ("PullRequest", _) if n["isDraft"] == true => "draft",
        ("Issue", "CLOSED")
            if matches!(n["stateReason"].as_str(), Some("NOT_PLANNED" | "DUPLICATE")) =>
        {
            "not_planned"
        }
        ("Issue", "CLOSED") => "closed",
        _ => "open",
    })
}

/// `owner/repo#123` for issue and PR API URLs.
fn thread_key(api: &str) -> Option<String> {
    let mut it = api.strip_prefix(API)?.strip_prefix("repos/")?.split('/');
    let (owner, repo, kind, num) = (it.next()?, it.next()?, it.next()?, it.next()?);
    matches!(kind, "issues" | "pulls").then(|| format!("{owner}/{repo}#{num}").to_lowercase())
}

fn html_url(n: &Note) -> String {
    let repo = &n.repository.html_url;
    let api = n
        .subject
        .url
        .as_deref()
        .and_then(|u| u.strip_prefix(API)?.strip_prefix("repos/"));
    if let Some([_, _, kind, id]) = api.map(|a| a.splitn(4, '/').collect::<Vec<_>>()).as_deref() {
        match *kind {
            "issues" => return format!("{repo}/issues/{id}"),
            "pulls" => return format!("{repo}/pull/{id}"),
            "commits" => return format!("{repo}/commit/{id}"),
            _ => {}
        }
    }
    match n.subject.kind.as_str() {
        "Release" => format!("{repo}/releases"),
        "CheckSuite" => format!("{repo}/actions"),
        "Discussion" => format!("{repo}/discussions"),
        _ => repo.clone(),
    }
}

fn next_page(link: &str) -> Option<String> {
    let part = link.split(',').find(|p| p.contains("rel=\"next\""))?;
    part.split(['<', '>'])
        .nth(1)?
        .strip_prefix(API)
        .map(str::to_owned)
}

fn last_page(link: &str) -> Option<u32> {
    link.split(',')
        .find(|p| p.contains("rel=\"last\""))?
        .split(['?', '&', '>'])
        .find_map(|kv| kv.strip_prefix("page="))?
        .parse()
        .ok()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `gh auth token`; GUI apps don't inherit the shell PATH, so try Homebrew paths first.
fn gh_token() -> Result<String> {
    let bin = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("gh");
    let out = Command::new(bin)
        .args(["auth", "token"])
        .output()
        .context("gh not found; set GITHUB_TOKEN")?;
    ensure!(
        out.status.success(),
        "gh auth token: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys_and_links() {
        assert_eq!(
            super::thread_key("https://api.github.com/repos/Foo/Bar/pulls/12").as_deref(),
            Some("foo/bar#12")
        );
        assert_eq!(
            super::thread_key("https://api.github.com/repos/foo/bar/releases/1"),
            None
        );
        let link = r#"<https://api.github.com/notifications?per_page=50&page=2>; rel="next", <https://api.github.com/notifications?per_page=50&page=7>; rel="last""#;
        assert_eq!(super::last_page(link), Some(7));
    }
}
