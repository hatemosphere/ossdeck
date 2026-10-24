mod db;
mod github;
mod mac;
mod store;
mod time;
mod ui;

use std::borrow::Cow;
use std::rc::Rc;

use futures::StreamExt as _;

use gpui::{
    Action, App, AppContext as _, Application, AssetSource, Bounds, Entity, Global, KeyBinding,
    Menu, MenuItem, SharedString, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};

use mac::TrayCmd;
use store::{INTERVALS, Store};
use ui::*;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$((concat!("icons/", $name, ".svg"), include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*]
    };
}

const ICONS: &[(&str, &[u8])] = icons!(
    "bell",
    "check",
    "command-palette",
    "comment-discussion",
    "eye",
    "git-commit",
    "git-merge",
    "git-pull-request",
    "git-pull-request-closed",
    "git-pull-request-draft",
    "issue-closed",
    "issue-opened",
    "link-external",
    "shield",
    "skip",
    "sync",
    "tag",
    "tasklist",
    "workflow",
);

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, b)| Cow::Borrowed(*b)))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ICONS.iter().map(|(p, _)| SharedString::from(*p)).collect())
    }
}

struct StoreGlobal(Entity<Store>);

impl Global for StoreGlobal {}

fn store(cx: &App) -> Entity<Store> {
    cx.global::<StoreGlobal>().0.clone()
}

fn open_window(cx: &mut App) {
    let store = store(cx);
    let bounds = Bounds::centered(None, size(px(820.), px(640.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // The header doubles as the title bar; ui::TRAFFIC_LIGHTS_W keeps the window buttons clear of it.
        titlebar: Some(TitlebarOptions {
            title: Some("ossdeck".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(14.), px(14.))),
        }),
        ..Default::default()
    };
    if let Err(e) = cx.open_window(options, |window, cx| {
        cx.new(|cx| Deck::new(store, window, cx))
    }) {
        eprintln!("open window: {e:#}");
    }
    mac::set_dock_visible(true);
    cx.activate(true);
}

/// Brings the existing window forward, or reopens it after it was closed.
fn show_window(cx: &mut App) {
    match cx.windows().first() {
        Some(w) => {
            w.update(cx, |_, window, _| window.activate_window()).ok();
            cx.activate(true);
        }
        None => open_window(cx),
    }
}

fn sync_status(s: &Store) -> String {
    match (&s.error, s.syncing, s.synced_at) {
        (_, true, _) => "Syncing...".into(),
        (Some(e), _, _) => format!("Sync error: {}", e.chars().take(60).collect::<String>()),
        (_, _, Some(t)) if time::now() - t < 60 => "Synced just now".into(),
        (_, _, Some(t)) => format!("Synced {} ago", time::ago(t)),
        _ => "Not synced yet".into(),
    }
}

fn tray_cmd(cmd: TrayCmd, cx: &mut App) {
    match cmd {
        TrayCmd::Open => show_window(cx),
        TrayCmd::Sync => store(cx).update(cx, |s, cx| s.sync(true, cx)),
        TrayCmd::Quit => cx.quit(),
        TrayCmd::Thread(id) => {
            let store = store(cx);
            let url = store
                .read(cx)
                .rows
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.url.clone());
            if let Some(url) = url {
                cx.open_url(&url);
                store.update(cx, |s, cx| s.opened(&id, cx));
            }
        }
    }
}

/// Menu bar item + Dock badge, refreshed whenever the store changes.
fn tray(store: &Entity<Store>, cx: &mut App) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let Some(tray) = mac::Tray::new(tx) else {
        return;
    };
    let tray = Rc::new(tray);
    let refresh = move |store: Entity<Store>, cx: &mut App| {
        let unread: Vec<_> = store
            .read(cx)
            .rows
            .iter()
            .filter(|t| t.unread && !t.done)
            .cloned()
            .collect();
        let status = sync_status(&store.read(cx));
        let items: Vec<(String, String)> = unread
            .iter()
            .take(10)
            .map(|t| {
                let repo = t.repo.rsplit('/').next().unwrap_or(&t.repo);
                let mut label: String = format!("{repo}: {}", t.title).chars().take(72).collect();
                if label.chars().count() == 72 {
                    label.push_str("...");
                }
                (t.id.clone(), label)
            })
            .collect();
        tray.update(unread.len(), &status, &items);
    };
    refresh(store.clone(), cx);
    let refresh = Rc::new(refresh);
    cx.observe(store, {
        let refresh = refresh.clone();
        move |store, cx| refresh(store, cx)
    })
    .detach();
    // Keeps "Synced 2m ago" current between syncs.
    let store = store.clone();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(30))
                .await;
            if cx.update(|cx| refresh(store.clone(), cx)).is_err() {
                break;
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Some(cmd) = rx.next().await {
            if cx.update(|cx| tray_cmd(cmd, cx)).is_err() {
                break;
            }
        }
    })
    .detach();
}

fn menus(cx: &mut App) {
    let current = store(cx).read(cx).interval;
    let actions: [Box<dyn Action>; 4] = [
        Box::new(Every1m),
        Box::new(Every5m),
        Box::new(Every15m),
        Box::new(Every30m),
    ];
    let every = INTERVALS
        .iter()
        .zip(actions)
        .map(|(&(secs, label), action)| MenuItem::Action {
            name: if secs == current {
                format!("{label} (current)").into()
            } else {
                label.into()
            },
            action,
            os_action: None,
        })
        .collect();
    let open_read = if store(cx).read(cx).open_marks_read {
        "Stop Marking Read on Open"
    } else {
        "Mark Read on Open"
    };
    let login = if mac::login_enabled() {
        "Disable Launch at Login"
    } else {
        "Enable Launch at Login"
    };
    cx.set_menus(vec![Menu {
        name: "ossdeck".into(),
        items: vec![
            MenuItem::action("Command Palette", OpenPalette),
            MenuItem::action("Refresh", Refresh),
            MenuItem::submenu(Menu {
                name: "Sync Every".into(),
                items: every,
            }),
            MenuItem::action(open_read, ToggleOpenRead),
            MenuItem::action(login, ToggleLogin),
            MenuItem::separator(),
            MenuItem::action("Quit ossdeck", Quit),
        ],
    }]);
}

fn set_interval(secs: u64, cx: &mut App) {
    store(cx).update(cx, |s, cx| s.set_interval(secs, cx));
    menus(cx);
}

fn main() {
    let app = Application::new().with_assets(Assets);
    // Closing the window keeps syncing in the background; the dock icon reopens it.
    app.on_reopen(show_window);
    app.run(|cx: &mut App| {
        mac::init_notifications();
        let entity = match Store::new(cx) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ossdeck: {e:#}");
                return cx.quit();
            }
        };
        cx.set_global(StoreGlobal(entity.clone()));
        tray(&entity, cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                mac::set_dock_visible(false);
            }
        })
        .detach();

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Refresh, cx| store(cx).update(cx, |s, cx| s.sync(true, cx)));
        cx.on_action(|_: &ToggleLogin, cx| {
            if let Err(e) = mac::set_login(!mac::login_enabled()) {
                eprintln!("launch at login: {e}");
            }
            menus(cx);
        });
        cx.on_action(|_: &ToggleOpenRead, cx| {
            store(cx).update(cx, |s, cx| s.toggle_open_marks_read(cx));
            menus(cx);
        });
        cx.on_action(|_: &Every1m, cx| set_interval(INTERVALS[0].0, cx));
        cx.on_action(|_: &Every5m, cx| set_interval(INTERVALS[1].0, cx));
        cx.on_action(|_: &Every15m, cx| set_interval(INTERVALS[2].0, cx));
        cx.on_action(|_: &Every30m, cx| set_interval(INTERVALS[3].0, cx));

        let deck = Some("Deck");
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", Refresh, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("down", SelectNext, deck),
            KeyBinding::new("j", SelectNext, deck),
            KeyBinding::new("up", SelectPrev, deck),
            KeyBinding::new("k", SelectPrev, deck),
            KeyBinding::new("o", Open, deck),
            KeyBinding::new("enter", Open, deck),
            KeyBinding::new("r", MarkRead, deck),
            KeyBinding::new("e", MarkDone, deck),
            KeyBinding::new("t", ToggleTask, deck),
            KeyBinding::new("s", StepTask, deck),
            KeyBinding::new("u", ToggleUnread, deck),
            KeyBinding::new("1", TabInbox, deck),
            KeyBinding::new("2", TabMine, deck),
            KeyBinding::new("3", TabUpstream, deck),
            KeyBinding::new("4", TabTasks, deck),
            KeyBinding::new("p", PickPriority, deck),
            KeyBinding::new("cmd-k", OpenPalette, deck),
            KeyBinding::new("i", PickInterval, deck),
            KeyBinding::new("f", CycleFilter, deck),
            KeyBinding::new("shift-s", ToggleSort, deck),
        ]);
        menus(cx);
        open_window(cx);
    });
}
