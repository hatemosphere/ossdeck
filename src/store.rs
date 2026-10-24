use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use gpui::{App, AppContext as _, Context, Entity, Task};
use rusqlite::Connection;

use crate::db::{self, Thread};
use crate::github::{self, Github};
use crate::{mac, time};

/// Sync interval choices in seconds. GitHub asks notification pollers to wait
/// X-Poll-Interval (60s) between requests, so a minute is the floor.
pub const INTERVALS: [(u64, &str); 4] = [
    (60, "1 minute"),
    (300, "5 minutes"),
    (900, "15 minutes"),
    (1800, "30 minutes"),
];

pub struct Store {
    db: Connection,
    sync_db: Arc<Mutex<Connection>>,
    gh: Option<Arc<Github>>,
    pub rows: Vec<Rc<Thread>>,
    pub syncing: bool,
    pub synced_at: Option<i64>,
    pub error: Option<String>,
    pub interval: u64,
    pub next_at: Option<i64>,
    /// Off by default: opening a thread leaves it unread so it stays in the Unread view.
    pub open_marks_read: bool,
    pub me: String,
    /// Orgs you can admin, and the ones you opted in as "mine".
    pub admin_orgs: Vec<String>,
    pub mine_orgs: Vec<String>,
    poll: Task<()>,
}

impl Store {
    pub fn new(cx: &mut App) -> Result<Entity<Self>> {
        let (db, sync_db) = (db::open()?, db::open()?);
        let interval = db::get(&db, "interval")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(INTERVALS[0].0);
        let open_marks_read = db::get(&db, "open_marks_read")?.as_deref() == Some("1");
        Ok(cx.new(|cx| {
            let mut store = Self {
                sync_db: Arc::new(Mutex::new(sync_db)),
                db,
                gh: None,
                rows: Vec::new(),
                syncing: false,
                synced_at: db::demo().map(|_| time::now() - 120),
                error: None,
                interval,
                next_at: db::demo().map(|_| time::now() + 180),
                open_marks_read,
                me: String::new(),
                admin_orgs: Vec::new(),
                mine_orgs: Vec::new(),
                poll: if db::demo().is_some() {
                    Task::ready(())
                } else {
                    Self::poll(cx)
                },
            };
            store.reload();
            store
        }))
    }

    fn poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while let Ok(wait) = this.update(cx, |s, cx| {
                s.sync(false, cx);
                let wait = s.wait();
                s.next_at = Some(time::now() + wait.as_secs() as i64);
                wait
            }) {
                cx.background_executor().timer(wait).await;
            }
        })
    }

    fn wait(&self) -> Duration {
        let floor = db::get(&self.db, "poll_interval")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        Duration::from_secs(self.interval.max(floor))
    }

    pub fn set_interval(&mut self, secs: u64, cx: &mut Context<Self>) {
        self.interval = secs;
        db::set(&self.db, "interval", &secs.to_string()).ok();
        self.poll = Self::poll(cx);
        cx.notify();
    }

    pub fn toggle_open_marks_read(&mut self, cx: &mut Context<Self>) {
        self.open_marks_read = !self.open_marks_read;
        db::set(
            &self.db,
            "open_marks_read",
            if self.open_marks_read { "1" } else { "0" },
        )
        .ok();
        cx.notify();
    }

    pub fn toggle_mine_org(&mut self, org: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.mine_orgs.iter().position(|o| o == org) {
            self.mine_orgs.remove(i);
        } else {
            self.mine_orgs.push(org.to_owned());
        }
        let saved = serde_json::to_string(&self.mine_orgs)
            .map_err(anyhow::Error::from)
            .and_then(|v| db::set(&self.db, "mine_orgs", &v))
            .and_then(|_| db::recompute_mine(&self.db));
        if let Err(e) = saved {
            self.error = Some(format!("{e:#}"));
        }
        self.reload();
        cx.notify();
    }

    fn reload(&mut self) {
        self.me = db::get(&self.db, "me").ok().flatten().unwrap_or_default();
        self.admin_orgs = db::get_list(&self.db, "admin_orgs").unwrap_or_default();
        self.mine_orgs = db::get_list(&self.db, "mine_orgs").unwrap_or_default();
        match db::load(&self.db, None) {
            Ok(rows) => self.rows = rows.into_iter().map(Rc::new).collect(),
            Err(e) => {
                eprintln!("load: {e:#}");
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    pub fn sync(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        self.syncing = true;
        cx.notify();
        let (gh, db) = (self.gh.clone(), self.sync_db.clone());
        let work = cx.background_executor().spawn(async move {
            let gh = match gh {
                Some(gh) => gh,
                None => Arc::new(Github::new()?),
            };
            let fresh = github::sync(&gh, &db.lock().expect("sync db"), force)?;
            anyhow::Ok((gh, fresh))
        });
        cx.spawn(async move |this, cx| {
            let res = work.await;
            this.update(cx, |s, cx| {
                s.syncing = false;
                match res {
                    Ok((gh, fresh)) => {
                        s.gh = Some(gh);
                        s.synced_at = Some(time::now());
                        s.error = None;
                        for t in &fresh {
                            mac::notify(&t.repo, &t.title, &github::describe(t), &t.url);
                        }
                    }
                    Err(e) => {
                        eprintln!("sync: {e:#}");
                        s.error = Some(format!("{e:#}"));
                    }
                }
                s.reload();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn local(&mut self, sql: &str, id: &str, cx: &mut Context<Self>) {
        if let Err(e) = self.db.execute(sql, [id]) {
            self.error = Some(e.to_string());
        }
        self.reload();
        cx.notify();
    }

    fn remote(&self, id: &str, done: bool, cx: &mut Context<Self>) {
        let (Some(gh), id) = (self.gh.clone(), id.to_owned()) else {
            return;
        };
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = if done {
                    gh.mark_done(&id)
                } else {
                    gh.mark_read(&id)
                } {
                    eprintln!("{e:#}");
                }
            })
            .detach();
    }

    pub fn mark_read(&mut self, id: &str, cx: &mut Context<Self>) {
        self.local("UPDATE threads SET unread = 0 WHERE id = ?", id, cx);
        self.remote(id, false, cx);
    }

    pub fn mark_done(&mut self, id: &str, cx: &mut Context<Self>) {
        self.local(
            "UPDATE threads SET done = 1, unread = 0 WHERE id = ?",
            id,
            cx,
        );
        self.remote(id, true, cx);
    }

    /// Opening a thread in your own repo counts as looking at it for its task.
    pub fn opened(&mut self, id: &str, cx: &mut Context<Self>) {
        self.db
            .execute(
                "UPDATE tasks SET stage = 1 WHERE id = ?1 AND stage = 0
                 AND EXISTS (SELECT 1 FROM threads WHERE id = ?1 AND mine = 1)",
                [id],
            )
            .ok();
        if self.open_marks_read {
            self.mark_read(id, cx);
        } else {
            self.reload();
            cx.notify();
        }
    }

    pub fn toggle_task(&mut self, id: &str, cx: &mut Context<Self>) {
        let deleted = self
            .db
            .execute("DELETE FROM tasks WHERE id = ?", [id])
            .unwrap_or(0);
        if deleted == 0 {
            self.local("INSERT INTO tasks (id) VALUES (?)", id, cx);
        } else {
            self.reload();
            cx.notify();
        }
    }

    pub fn set_priority(&mut self, id: &str, priority: u8, cx: &mut Context<Self>) {
        self.db
            .execute(
                "INSERT INTO tasks (id, priority) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET priority = ?2",
                (id, priority),
            )
            .ok();
        self.reload();
        cx.notify();
    }

    pub fn step_task(&mut self, id: &str, cx: &mut Context<Self>) {
        self.local(
            "UPDATE tasks SET stage = (stage + 1) % 3 WHERE id = ?",
            id,
            cx,
        );
    }
}
