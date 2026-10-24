use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyElement, AnyView, App, Context, Div, ElementId, Entity, FocusHandle, FontWeight,
    HighlightStyle, IntoElement, KeyContext, KeyDownEvent, MouseButton, Render, Rgba, ScrollHandle,
    ScrollStrategy, SharedString, Stateful, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, WindowAppearance, actions, div, prelude::*, px, rgb, rgba,
    svg, uniform_list,
};

use crate::db::Thread;
use crate::store::{INTERVALS, Store};
use crate::time;

actions!(
    ossdeck,
    [
        Quit,
        Refresh,
        ToggleLogin,
        Every1m,
        Every5m,
        Every15m,
        Every30m,
        SelectNext,
        SelectPrev,
        Open,
        MarkRead,
        MarkDone,
        ToggleTask,
        StepTask,
        ToggleUnread,
        TabInbox,
        TabMine,
        TabUpstream,
        TabTasks,
        Priority0,
        Priority1,
        Priority2,
        Priority3,
        Priority4,
        CycleFilter,
        ToggleSort,
        OpenPalette,
        PickPriority,
        PickInterval,
        ToggleOpenRead,
        CloseWindow
    ]
);

const ROW_H: f32 = 48.;
/// Room for the macOS window buttons, which sit inside the header.
pub const TRAFFIC_LIGHTS_W: f32 = 78.;
/// Priority icon + stage bars + stage label.
const TASK_COL_W: f32 = 14. + 12. + 3. * 10. + 2. * 4. + 4. + 88.;

#[derive(Clone, Copy)]
struct Theme {
    bg: Rgba,
    fg: Rgba,
    mute: Rgba,
    line: Rgba,
    sel: Rgba,
    hover: Rgba,
    raised: Rgba,
    overlay: Rgba,
    accent: Rgba,
    green: Rgba,
    purple: Rgba,
    red: Rgba,
    gray: Rgba,
}

impl Theme {
    // GitHub Primer colors.
    fn of(window: &Window) -> Self {
        match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self {
                bg: rgb(0x0d1117),
                fg: rgb(0xf0f6fc),
                mute: rgb(0x9198a1),
                line: rgb(0x3d444d),
                sel: rgb(0x1c2b3f),
                hover: rgb(0x151b23),
                raised: rgb(0x212830),
                overlay: rgba(0x01040966),
                accent: rgb(0x4493f8),
                green: rgb(0x3fb950),
                purple: rgb(0xab7df8),
                red: rgb(0xf85149),
                gray: rgb(0x9198a1),
            },
            _ => Self {
                bg: rgb(0xffffff),
                fg: rgb(0x1f2328),
                mute: rgb(0x59636e),
                line: rgb(0xd1d9e0),
                sel: rgb(0xddf4ff),
                hover: rgb(0xf6f8fa),
                raised: rgb(0xf6f8fa),
                overlay: rgba(0x1f232833),
                accent: rgb(0x0969da),
                green: rgb(0x1a7f37),
                purple: rgb(0x8250df),
                red: rgb(0xd1242f),
                gray: rgb(0x59636e),
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Inbox,
    Mine,
    Upstream,
    Tasks,
}

impl Tab {
    fn action(self) -> Box<dyn Action> {
        match self {
            Tab::Inbox => Box::new(TabInbox),
            Tab::Mine => Box::new(TabMine),
            Tab::Upstream => Box::new(TabUpstream),
            Tab::Tasks => Box::new(TabTasks),
        }
    }
}

const PRIORITIES: [&str; 5] = ["No priority", "Urgent", "High", "Medium", "Low"];

fn priority_action(p: u8) -> Box<dyn Action> {
    match p {
        1 => Box::new(Priority1),
        2 => Box::new(Priority2),
        3 => Box::new(Priority3),
        4 => Box::new(Priority4),
        _ => Box::new(Priority0),
    }
}

const TABS: [(Tab, &str); 4] = [
    (Tab::Inbox, "Inbox"),
    (Tab::Mine, "Mine"),
    (Tab::Upstream, "Upstream"),
    (Tab::Tasks, "Tasks"),
];

fn matches(tab: Tab, unread_only: bool, t: &Thread) -> bool {
    match tab {
        Tab::Tasks => t.stage.is_some(),
        Tab::Inbox => !t.done && (!unread_only || t.unread),
        Tab::Mine => !t.done && (!unread_only || t.unread) && t.mine,
        Tab::Upstream => !t.done && (!unread_only || t.unread) && !t.mine,
    }
}

/// The shortcut shown next to an action, taken from the live keymap so labels never drift.
fn keys_for(window: &Window, action: &dyn Action) -> Vec<SharedString> {
    let ctx = KeyContext::parse("Deck").expect("valid context");
    window
        .highest_precedence_binding_for_action_in_context(action, ctx)
        .map(|b| {
            b.keystrokes()
                .iter()
                .map(|k| k.to_string().replace("enter", "↵").into())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Commands,
    Priority,
    Interval,
}

pub fn interval_action(ix: usize) -> Box<dyn Action> {
    match ix {
        0 => Box::new(Every1m),
        1 => Box::new(Every5m),
        2 => Box::new(Every15m),
        _ => Box::new(Every30m),
    }
}

fn minutes(secs: i64) -> String {
    if secs < 60 {
        "<1m".into()
    } else {
        format!("{}m", (secs + 59) / 60)
    }
}

struct Palette {
    mode: Mode,
    query: String,
    selected: usize,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

struct Cmd {
    label: SharedString,
    action: Box<dyn Action>,
    icon: Option<AnyElement>,
    keys: Vec<SharedString>,
}

pub struct Deck {
    store: Entity<Store>,
    tab: Tab,
    unread_only: bool,
    selected: usize,
    /// Tasks tab only.
    priority_filter: Option<u8>,
    by_priority: bool,
    palette: Option<Palette>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    _subs: [Subscription; 2],
    /// Re-renders the "synced 2m ago / next in 3m" status between syncs.
    _tick: Task<()>,
}

impl Deck {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let _subs = [
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
        ];
        let mut deck = Self {
            store,
            tab: Tab::Inbox,
            unread_only: true,
            selected: 0,
            priority_filter: None,
            by_priority: true,
            palette: None,
            scroll: UniformListScrollHandle::new(),
            focus,
            _subs,
            _tick: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(30))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }),
        };
        // Starting view for README screenshots, e.g. OSSDECK_VIEW=tasks or OSSDECK_VIEW=palette.
        match std::env::var("OSSDECK_VIEW").as_deref() {
            Ok("all") => deck.unread_only = false,
            Ok("mine") => deck.tab = Tab::Mine,
            Ok("upstream") => deck.tab = Tab::Upstream,
            Ok("tasks") => deck.tab = Tab::Tasks,
            Ok("palette") => deck.open_palette(Mode::Commands, window, cx),
            Ok("priority") => {
                deck.tab = Tab::Tasks;
                deck.open_palette(Mode::Priority, window, cx);
            }
            Ok("interval") => deck.open_palette(Mode::Interval, window, cx),
            _ => {}
        }
        deck
    }

    fn visible(&self, cx: &App) -> Vec<Rc<Thread>> {
        let mut rows: Vec<_> = self
            .store
            .read(cx)
            .rows
            .iter()
            .filter(|t| matches(self.tab, self.unread_only, t))
            .cloned()
            .collect();
        if self.tab == Tab::Tasks {
            if let Some(p) = self.priority_filter {
                rows.retain(|t| t.priority == p);
            }
            if self.by_priority {
                // Finished tasks sink; then urgent first, no priority last. Stable, so recency breaks ties.
                rows.sort_by_key(|t| {
                    (
                        final_stage(t).is_some(),
                        if t.priority == 0 { 5 } else { t.priority },
                    )
                });
            }
        }
        rows
    }

    fn current(&self, cx: &App) -> Option<Rc<Thread>> {
        self.visible(cx).get(self.selected).cloned()
    }

    fn cycle_filter(&mut self, cx: &mut Context<Self>) {
        self.priority_filter = match self.priority_filter {
            None => Some(1),
            Some(4) => Some(0),
            Some(0) => None,
            Some(p) => Some(p + 1),
        };
        self.set_tab(Tab::Tasks, cx);
    }

    fn priority(&mut self, p: u8, cx: &mut Context<Self>) {
        self.act(cx, |s, id, cx| s.set_priority(id, p, cx));
    }

    fn select(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.visible(cx).len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Top);
        cx.notify();
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.selected = 0;
        cx.notify();
    }

    fn toggle_unread(&mut self, cx: &mut Context<Self>) {
        self.unread_only = !self.unread_only;
        self.selected = 0;
        cx.notify();
    }

    fn toggle_sort(&mut self, cx: &mut Context<Self>) {
        self.by_priority = !self.by_priority;
        cx.notify();
    }

    fn act(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Store, &str, &mut Context<Store>),
    ) {
        if let Some(t) = self.current(cx) {
            self.store.update(cx, |s, cx| f(s, &t.id, cx));
        }
    }

    fn open(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(t) = self.visible(cx).get(ix).cloned() {
            self.selected = ix;
            cx.open_url(&t.url);
            self.store.update(cx, |s, cx| s.opened(&t.id, cx));
        }
    }

    fn open_palette(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if mode == Mode::Priority && self.current(cx).is_none() {
            return;
        }
        let focus = cx.focus_handle();
        window.focus(&focus);
        self.palette = Some(Palette {
            mode,
            query: String::new(),
            selected: 0,
            focus,
            scroll: ScrollHandle::new(),
        });
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Palette entries run the same actions as the hotkeys and buttons.
    fn run(&mut self, action: Box<dyn Action>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        window.dispatch_action(action, cx);
    }

    fn commands(&self, mode: Mode, window: &Window, cx: &App) -> Vec<Cmd> {
        let th = Theme::of(window);
        let cmd = |label: &str, action: Box<dyn Action>, icon: Option<&'static str>| Cmd {
            label: label.to_owned().into(),
            keys: keys_for(window, &*action),
            icon: icon.map(|i| {
                svg()
                    .path(i)
                    .size(px(14.))
                    .text_color(th.mute)
                    .into_any_element()
            }),
            action,
        };
        if mode == Mode::Interval {
            let current = self.store.read(cx).interval;
            return INTERVALS
                .iter()
                .enumerate()
                .map(|(i, &(secs, label))| Cmd {
                    label: format!("Every {label}").into(),
                    action: interval_action(i),
                    icon: (secs == current).then(|| {
                        svg()
                            .path("icons/check.svg")
                            .size(px(14.))
                            .text_color(th.accent)
                            .into_any_element()
                    }),
                    keys: vec![(i + 1).to_string().into()],
                })
                .collect();
        }
        if mode == Mode::Priority {
            return (0..5u8)
                .map(|p| Cmd {
                    label: PRIORITIES[p as usize].into(),
                    action: priority_action(p),
                    icon: Some(priority_icon(p, th).into_any_element()),
                    keys: vec![p.to_string().into()],
                })
                .collect();
        }
        let mut v = Vec::new();
        if let Some(t) = self.current(cx) {
            let tracked = t.stage.is_some();
            v.push(cmd(
                "Open in browser",
                Box::new(Open),
                Some("icons/link-external.svg"),
            ));
            if t.unread {
                v.push(cmd(
                    "Mark as read",
                    Box::new(MarkRead),
                    Some("icons/eye.svg"),
                ));
            }
            v.push(cmd(
                "Mark as done",
                Box::new(MarkDone),
                Some("icons/check.svg"),
            ));
            v.push(cmd(
                if tracked {
                    "Stop tracking"
                } else {
                    "Track as task"
                },
                Box::new(ToggleTask),
                Some("icons/tasklist.svg"),
            ));
            if tracked {
                v.push(cmd(
                    "Move task to next stage",
                    Box::new(StepTask),
                    Some("icons/tasklist.svg"),
                ));
            }
            v.push(Cmd {
                icon: Some(priority_icon(t.priority, th).into_any_element()),
                ..cmd("Set priority...", Box::new(PickPriority), None)
            });
        }
        for (tab, name) in TABS {
            v.push(cmd(&format!("Go to {name}"), tab.action(), None));
        }
        v.push(cmd(
            if self.unread_only {
                "Show read threads too"
            } else {
                "Show unread only"
            },
            Box::new(ToggleUnread),
            None,
        ));
        v.push(cmd("Filter tasks by priority", Box::new(CycleFilter), None));
        v.push(cmd(
            if self.by_priority {
                "Sort tasks by recent"
            } else {
                "Sort tasks by priority"
            },
            Box::new(ToggleSort),
            None,
        ));
        v.push(cmd("Sync now", Box::new(Refresh), Some("icons/sync.svg")));
        let every = minutes(self.store.read(cx).interval as i64);
        v.push(cmd(
            &format!("Change sync interval (now every {every})..."),
            Box::new(PickInterval),
            Some("icons/sync.svg"),
        ));
        let open_read = self.store.read(cx).open_marks_read;
        v.push(cmd(
            if open_read {
                "Stop marking threads read when opening"
            } else {
                "Mark threads read when opening"
            },
            Box::new(ToggleOpenRead),
            None,
        ));
        v.push(cmd("Toggle launch at login", Box::new(ToggleLogin), None));
        v.push(cmd("Quit ossdeck", Box::new(Quit), None));
        v
    }

    fn filtered(&self, window: &Window, cx: &App) -> Vec<Cmd> {
        let Some(p) = &self.palette else {
            return Vec::new();
        };
        let q = p.query.to_lowercase();
        let mut cmds = self.commands(p.mode, window, cx);
        cmds.retain(|c| {
            let label = c.label.to_lowercase();
            q.split_whitespace().all(|w| label.contains(w))
        });
        cmds
    }

    fn palette_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
            return;
        }
        let n = self.filtered(window, cx).len();
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        cx.stop_propagation();
        match k.key.as_str() {
            "escape" => return self.close_palette(window, cx),
            "enter" => {
                let ix = p.selected;
                if let Some(c) = self.filtered(window, cx).into_iter().nth(ix) {
                    self.run(c.action, window, cx);
                }
                return;
            }
            "up" => p.selected = p.selected.saturating_sub(1),
            "down" => p.selected = (p.selected + 1).min(n.saturating_sub(1)),
            "backspace" => {
                p.query.pop();
                p.selected = 0;
            }
            _ => {
                let Some(c) = k
                    .key_char
                    .clone()
                    .filter(|c| !c.chars().any(char::is_control))
                else {
                    return;
                };
                if p.mode == Mode::Priority
                    && let Ok(d @ 0..=4) = c.parse::<u8>()
                {
                    return self.run(priority_action(d), window, cx);
                }
                if p.mode == Mode::Interval
                    && let Ok(d @ 1..=4) = c.parse::<usize>()
                {
                    return self.run(interval_action(d - 1), window, cx);
                }
                p.query.push_str(&c);
                p.selected = 0;
            }
        }
        p.scroll.scroll_to_item(p.selected);
        cx.notify();
    }

    fn header(&self, th: Theme, cx: &mut Context<Self>) -> Div {
        let store = self.store.read(cx);
        let count = |tab| {
            store
                .rows
                .iter()
                .filter(|t| {
                    matches(tab, true, t) && (tab != Tab::Tasks || final_stage(t).is_none())
                })
                .count()
        };
        let (status, status_color): (SharedString, _) =
            match (&store.error, store.syncing, store.synced_at) {
                (_, true, _) => ("syncing...".into(), th.mute),
                (Some(e), _, _) => (format!("error: {e}").into(), th.red),
                (_, _, Some(t)) if time::now() - t < 60 => ("synced just now".into(), th.mute),
                (_, _, Some(t)) => (format!("synced {} ago", time::ago(t)).into(), th.mute),
                _ => ("not synced yet".into(), th.mute),
            };
        let next = store
            .next_at
            .filter(|_| !store.syncing)
            .map(|t| format!("next in {}", minutes((t - time::now()).max(0))));
        let every = format!("every {}", minutes(store.interval as i64));
        let sep = || div().w(px(1.)).h(px(12.)).bg(th.line);
        let tabs = TABS.map(|(tab, name)| {
            let n = count(tab);
            let on = tab == self.tab;
            div()
                .id(name)
                .flex_none()
                .whitespace_nowrap()
                .px_2()
                .py_1()
                .rounded(px(6.))
                .text_sm()
                .cursor_pointer()
                .text_color(if on { th.fg } else { th.mute })
                .when(on, |d| d.bg(th.sel).font_weight(FontWeight::MEDIUM))
                .hover(|s| s.bg(th.hover))
                .tooltip(tip(format!("Go to {name}"), tab.action()))
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                .child(if n > 0 {
                    format!("{name} {n}")
                } else {
                    name.to_owned()
                })
        });
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .pr_3()
            .h(px(40.))
            .border_b_1()
            .border_color(th.line)
            .child(drag_area().flex_none().w(px(TRAFFIC_LIGHTS_W)))
            .children(tabs)
            .child(drag_area().flex_1())
            .child(
                div()
                    .id("sync-status")
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(th.mute)
                    .hover(|s| s.bg(th.hover))
                    .tooltip(tip("Change sync interval", Box::new(PickInterval)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_palette(Mode::Interval, window, cx)
                    }))
                    .child(
                        svg()
                            .path("icons/sync.svg")
                            .size(px(12.))
                            .text_color(status_color),
                    )
                    .child(
                        div()
                            .max_w(px(260.))
                            .truncate()
                            .text_color(status_color)
                            .child(status),
                    )
                    .children(
                        next.map(|n| div().flex().items_center().gap_2().child(sep()).child(n)),
                    )
                    .child(sep())
                    .child(every),
            )
            .child(
                chip("scope", if self.unread_only { "Unread" } else { "All" }, th)
                    .tooltip(tip("Toggle unread only", Box::new(ToggleUnread)))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_unread(cx))),
            )
    }

    /// Filter and sort for the Tasks tab, in its own row so the header never overflows.
    fn tasks_bar(&self, th: Theme, cx: &mut Context<Self>) -> Div {
        let filter = self
            .priority_filter
            .map_or("Any priority", |p| PRIORITIES[p as usize]);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(36.))
            .border_b_1()
            .border_color(th.line)
            .text_xs()
            .text_color(th.mute)
            .child("Priority")
            .child(
                chip("filter", filter, th)
                    .tooltip(tip("Filter by priority", Box::new(CycleFilter)))
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_filter(cx))),
            )
            .child(div().w(px(8.)))
            .child("Sort")
            .child(
                chip(
                    "sort",
                    if self.by_priority {
                        "By priority"
                    } else {
                        "By recent"
                    },
                    th,
                )
                .tooltip(tip("Change sort", Box::new(ToggleSort)))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_sort(cx))),
            )
    }

    /// Context-aware actions for the selected thread, each labelled with its hotkey.
    fn action_bar(&self, current: Option<&Thread>, window: &Window, th: Theme) -> Div {
        let mut left: Vec<Stateful<Div>> = Vec::new();
        if let Some(t) = current {
            let tracked = t.stage.is_some();
            left.push(button("a-open", "Open", Box::new(Open), window, th));
            if t.unread {
                left.push(button("a-read", "Read", Box::new(MarkRead), window, th));
            }
            left.push(button("a-done", "Done", Box::new(MarkDone), window, th));
            left.push(button(
                "a-track",
                if tracked { "Untrack" } else { "Track" },
                Box::new(ToggleTask),
                window,
                th,
            ));
            if tracked {
                left.push(button(
                    "a-step",
                    "Next stage",
                    Box::new(StepTask),
                    window,
                    th,
                ));
            }
            left.push(button(
                "a-prio",
                "Priority",
                Box::new(PickPriority),
                window,
                th,
            ));
        }
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(36.))
            .border_t_1()
            .border_color(th.line)
            .children(left)
            .child(div().flex_1())
            .child(button("a-sync", "Sync", Box::new(Refresh), window, th))
            .child(button(
                "a-cmd",
                "Commands",
                Box::new(OpenPalette),
                window,
                th,
            ))
    }

    fn palette_view(&self, p: &Palette, window: &Window, cx: &mut Context<Self>, th: Theme) -> Div {
        let cmds = self.filtered(window, cx);
        let selected = p.selected.min(cmds.len().saturating_sub(1));
        let context = match p.mode {
            Mode::Interval => Some(
                "Sync interval. GitHub asks clients to poll notifications at most once a minute."
                    .to_owned(),
            ),
            Mode::Priority => self
                .current(cx)
                .map(|t| format!("Set priority - {}", t.title)),
            Mode::Commands => self.current(cx).map(|t| t.title.clone()),
        };
        let placeholder = match p.mode {
            Mode::Commands => "Type a command...",
            Mode::Priority => "Pick a priority, or press 0-4...",
            Mode::Interval => "Pick an interval, or press 1-4...",
        };
        let items: Vec<_> = cmds
            .into_iter()
            .enumerate()
            .map(|(ix, c)| {
                let action = c.action;
                div()
                    .id(("cmd", ix))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(32.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_sm()
                    .when(ix == selected, |d| d.bg(th.sel))
                    .when(ix != selected, |d| d.hover(|s| s.bg(th.hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.run(action.boxed_clone(), window, cx)
                    }))
                    .child(
                        div()
                            .size(px(16.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .children(c.icon),
                    )
                    .child(div().flex_1().truncate().child(c.label))
                    .children(c.keys.into_iter().map(|k| kbd(k, th)))
            })
            .collect();
        let empty = items.is_empty();
        div()
            .absolute()
            .inset_0()
            .bg(th.overlay)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(64.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
            )
            .child(
                div()
                    .id("palette")
                    .track_focus(&p.focus)
                    .key_context("Palette")
                    .on_key_down(cx.listener(Self::palette_key))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .bg(th.bg)
                    .border_1()
                    .border_color(th.line)
                    .rounded(px(10.))
                    .shadow_lg()
                    .overflow_hidden()
                    .children(context.map(|c| {
                        div()
                            .px_3()
                            .pt_2()
                            .text_xs()
                            .text_color(th.mute)
                            .truncate()
                            .child(c)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .h(px(44.))
                            .border_b_1()
                            .border_color(th.line)
                            .text_sm()
                            .child(
                                svg()
                                    .path("icons/command-palette.svg")
                                    .size(px(14.))
                                    .text_color(th.mute),
                            )
                            .when(!p.query.is_empty(), |d| d.child(p.query.clone()))
                            .child(div().w(px(1.5)).h(px(16.)).bg(th.accent))
                            .when(p.query.is_empty(), |d| {
                                d.child(div().text_color(th.mute).child(placeholder))
                            }),
                    )
                    .child(
                        div()
                            .id("palette-list")
                            .track_scroll(&p.scroll)
                            .max_h(px(360.))
                            .overflow_y_scroll()
                            .p_1()
                            .flex()
                            .flex_col()
                            .children(items)
                            .when(empty, |d| {
                                d.child(
                                    div()
                                        .p_3()
                                        .text_sm()
                                        .text_color(th.mute)
                                        .child("No matching commands"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .h(px(30.))
                            .border_t_1()
                            .border_color(th.line)
                            .text_xs()
                            .text_color(th.mute)
                            .child(hint(&["↑", "↓"], "navigate", th))
                            .child(hint(&["↵"], "run", th))
                            .child(hint(&["esc"], "close", th)),
                    ),
            )
    }
}

impl Render for Deck {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let th = Theme::of(window);
        let rows = self.visible(cx);
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        let current = rows.get(self.selected).cloned();
        let unread = self
            .store
            .read(cx)
            .rows
            .iter()
            .filter(|t| t.unread && !t.done)
            .count();
        window.set_window_title(&if unread > 0 {
            format!("ossdeck ({unread})")
        } else {
            "ossdeck".into()
        });

        let body = if rows.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(th.mute)
                .child(if self.tab == Tab::Tasks {
                    "No tasks yet. Track a thread to see it here."
                } else {
                    "Inbox zero."
                })
                .into_any_element()
        } else {
            let (view, selected) = (cx.entity(), self.selected);
            uniform_list("threads", rows.len(), move |range, _, _| {
                range
                    .map(|ix| row(&rows[ix], ix, ix == selected, th, view.clone()))
                    .collect()
            })
            .track_scroll(self.scroll.clone())
            .flex_1()
            .w_full()
            .into_any_element()
        };

        let main = div()
            .track_focus(&self.focus)
            .key_context("Deck")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select(1, cx)))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.select(-1, cx)))
            .on_action(cx.listener(|this, _: &Open, _, cx| this.open(this.selected, cx)))
            .on_action(cx.listener(|this, _: &MarkRead, _, cx| this.act(cx, Store::mark_read)))
            .on_action(cx.listener(|this, _: &MarkDone, _, cx| this.act(cx, Store::mark_done)))
            .on_action(cx.listener(|this, _: &ToggleTask, _, cx| this.act(cx, Store::toggle_task)))
            .on_action(cx.listener(|this, _: &StepTask, _, cx| this.act(cx, Store::step_task)))
            .on_action(cx.listener(|this, _: &ToggleUnread, _, cx| this.toggle_unread(cx)))
            .on_action(cx.listener(|this, _: &TabInbox, _, cx| this.set_tab(Tab::Inbox, cx)))
            .on_action(cx.listener(|this, _: &TabMine, _, cx| this.set_tab(Tab::Mine, cx)))
            .on_action(cx.listener(|this, _: &TabUpstream, _, cx| this.set_tab(Tab::Upstream, cx)))
            .on_action(cx.listener(|this, _: &TabTasks, _, cx| this.set_tab(Tab::Tasks, cx)))
            .on_action(cx.listener(|this, _: &Priority0, _, cx| this.priority(0, cx)))
            .on_action(cx.listener(|this, _: &Priority1, _, cx| this.priority(1, cx)))
            .on_action(cx.listener(|this, _: &Priority2, _, cx| this.priority(2, cx)))
            .on_action(cx.listener(|this, _: &Priority3, _, cx| this.priority(3, cx)))
            .on_action(cx.listener(|this, _: &Priority4, _, cx| this.priority(4, cx)))
            .on_action(cx.listener(|this, _: &CycleFilter, _, cx| this.cycle_filter(cx)))
            .on_action(cx.listener(|this, _: &ToggleSort, _, cx| this.toggle_sort(cx)))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.open_palette(Mode::Commands, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PickPriority, window, cx| {
                this.open_palette(Mode::Priority, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PickInterval, window, cx| {
                this.open_palette(Mode::Interval, window, cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(th, cx))
            .when(self.tab == Tab::Tasks, |d| d.child(self.tasks_bar(th, cx)))
            .child(body)
            .children(current.as_deref().map(|t| preview(t, th)))
            .child(self.action_bar(current.as_deref(), window, th));

        let palette = self
            .palette
            .as_ref()
            .map(|p| self.palette_view(p, window, cx, th));
        div()
            .relative()
            .size_full()
            .bg(th.bg)
            .text_color(th.fg)
            .child(main)
            .children(palette)
    }
}

struct Tip {
    label: SharedString,
    keys: Vec<SharedString>,
    th: Theme,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let th = self.th;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(th.raised)
            .border_1()
            .border_color(th.line)
            .shadow_md()
            .text_xs()
            .text_color(th.fg)
            .child(div().max_w(px(460.)).child(self.label.clone()))
            .children(self.keys.iter().map(|k| kbd(k.clone(), th)))
    }
}

/// Plain text tooltip, wrapped, e.g. the full latest comment.
fn text_tip(text: String) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = SharedString::from(text);
    move |window, cx| {
        let th = Theme::of(window);
        cx.new(|_| Tip {
            label: text.clone(),
            keys: Vec::new(),
            th,
        })
        .into()
    }
}

/// Tooltip with the action's name and its shortcut.
fn tip(
    label: impl Into<SharedString>,
    action: Box<dyn Action>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let label = label.into();
    move |window, cx| {
        let (keys, th) = (keys_for(window, &*action), Theme::of(window));
        cx.new(|_| Tip {
            label: label.clone(),
            keys,
            th,
        })
        .into()
    }
}

/// Empty header space that moves the window (double-click zooms), like a native title bar.
fn drag_area() -> Div {
    div()
        .h_full()
        .on_mouse_down(MouseButton::Left, |ev, window, _| {
            if ev.click_count == 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}

fn kbd(key: SharedString, th: Theme) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .min_w(px(18.))
        .h(px(18.))
        .px(px(4.))
        .rounded(px(4.))
        .border_1()
        .border_color(th.line)
        .bg(th.raised)
        .text_size(px(11.))
        .text_color(th.mute)
        .child(key)
}

fn hint(keys: &[&'static str], label: &'static str, th: Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .children(keys.iter().map(|k| kbd((*k).into(), th)))
        .child(label)
}

/// Text button that shows its shortcut inline, Raycast/Linear style.
fn button(
    id: &'static str,
    label: &'static str,
    action: Box<dyn Action>,
    window: &Window,
    th: Theme,
) -> Stateful<Div> {
    let keys = keys_for(window, &*action);
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .h(px(26.))
        .px_2()
        .rounded(px(6.))
        .cursor_pointer()
        .text_xs()
        .text_color(th.fg)
        .hover(|s| s.bg(th.hover))
        .child(label)
        .children(keys.into_iter().map(|k| kbd(k, th)))
        .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
}

/// Icon button on a row: selects the row, then runs the action. Tooltip shows the shortcut.
fn row_button(
    id: impl Into<ElementId>,
    icon: AnyElement,
    label: &'static str,
    action: Box<dyn Action>,
    ix: usize,
    view: Entity<Deck>,
    th: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(th.line))
        .tooltip(tip(label, action.boxed_clone()))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            view.update(cx, |d, cx| {
                d.selected = ix;
                cx.notify();
            });
            window.dispatch_action(action.boxed_clone(), cx);
        })
        .child(icon)
}

fn chip(id: &'static str, label: &'static str, th: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .whitespace_nowrap()
        .px_2()
        .py_1()
        .rounded(px(6.))
        .text_xs()
        .cursor_pointer()
        .border_1()
        .border_color(th.line)
        .hover(|s| s.bg(th.hover))
        .child(label)
}

fn icon(path: &'static str, th: Theme) -> AnyElement {
    svg()
        .path(path)
        .size(px(14.))
        .text_color(th.mute)
        .into_any_element()
}

fn row(
    t: &Thread,
    ix: usize,
    selected: bool,
    th: Theme,
    view: Entity<Deck>,
) -> impl IntoElement + use<> {
    let (status, color) = status_icon(t, th);
    let tracked = t.stage.is_some();
    let open = view.clone();
    // Hover actions float over the right edge instead of reserving a column.
    let buttons = div()
        .absolute()
        .right(px(8.))
        .top(px((ROW_H - 28.) / 2.))
        .flex()
        .gap(px(2.))
        .p(px(2.))
        .rounded(px(6.))
        .bg(if selected { th.sel } else { th.hover })
        .opacity(0.)
        .group_hover("row", |s| s.opacity(1.))
        .when(t.unread, |d| {
            d.child(row_button(
                ("rb-read", ix),
                icon("icons/eye.svg", th),
                "Mark as read",
                Box::new(MarkRead),
                ix,
                view.clone(),
                th,
            ))
        })
        .child(row_button(
            ("rb-track", ix),
            icon("icons/tasklist.svg", th),
            if tracked {
                "Stop tracking"
            } else {
                "Track as task"
            },
            Box::new(ToggleTask),
            ix,
            view.clone(),
            th,
        ))
        .child(row_button(
            ("rb-prio", ix),
            priority_icon(t.priority, th).into_any_element(),
            "Set priority",
            Box::new(PickPriority),
            ix,
            view.clone(),
            th,
        ))
        .child(row_button(
            ("rb-done", ix),
            icon("icons/check.svg", th),
            "Mark as done",
            Box::new(MarkDone),
            ix,
            view,
            th,
        ));

    // Your own repos are always yours; the owner prefix only costs space.
    let repo = if t.mine {
        t.repo.rsplit('/').next().unwrap_or(&t.repo)
    } else {
        &t.repo
    };
    let event = t.event.clone().filter(|e| !e.is_empty());
    let (author, text) = match event.as_deref().and_then(|e| e.split_once(": ")) {
        Some((who, text)) => (Some(who.to_owned()), text.to_owned()),
        None => (
            None,
            event.clone().unwrap_or_else(|| t.reason.replace('_', " ")),
        ),
    };

    let title_line = div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_sm()
                .truncate()
                .text_color(if t.unread { th.fg } else { th.mute })
                .when(t.unread, |d| d.font_weight(FontWeight::MEDIUM))
                .child(t.title.clone()),
        )
        // Fixed widths so columns line up whether or not a row is tracked.
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_3()
                .w(px(TASK_COL_W))
                .when(tracked, |d| d.child(priority_icon(t.priority, th)))
                .children(progress(t, th)),
        )
        .child(
            div()
                .w(px(36.))
                .flex_none()
                .text_right()
                .text_xs()
                .text_color(th.mute)
                .child(time::parse(&t.updated).map(time::ago).unwrap_or_default()),
        );

    // The comment line runs the full row width; hovering it shows the whole comment.
    let meta_line = div()
        .id(("meta", ix))
        .flex()
        .gap_2()
        .text_xs()
        .text_color(th.mute)
        .child(div().flex_none().child(repo.to_owned()))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .child(comment(author, text, th)),
        )
        .when_some(event, |d, e| d.tooltip(text_tip(e)));

    div()
        .id(ix)
        .group("row")
        .relative()
        .w_full()
        .h(px(ROW_H))
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .border_b_1()
        .border_color(th.line)
        .cursor_pointer()
        .when(selected, |d| d.bg(th.sel))
        .when(!selected, |d| d.hover(|s| s.bg(th.hover)))
        .on_click(move |_, _, cx| open.update(cx, |d, cx| d.open(ix, cx)))
        .child(
            div()
                .size(px(8.))
                .flex_none()
                .rounded(px(4.))
                .when(t.unread, |d| d.bg(th.accent)),
        )
        .child(
            svg()
                .path(status)
                .size(px(16.))
                .flex_none()
                .text_color(color),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(title_line)
                .child(meta_line),
        )
        .child(buttons)
}

const PREVIEW_LINES: usize = 3;

/// Full latest comment of the selected thread, so long comments don't need a hover.
/// Fixed height keeps the list from jumping as the selection moves.
fn preview(t: &Thread, th: Theme) -> Div {
    let event = t.event.clone().filter(|e| !e.is_empty());
    let (author, text) = match event.as_deref().and_then(|e| e.split_once(": ")) {
        Some((who, text)) => (Some(who.to_owned()), text.to_owned()),
        None => (
            None,
            event.unwrap_or_else(|| format!("No comments yet - {}", t.reason.replace('_', " "))),
        ),
    };
    div()
        .flex_none()
        .h(px(PREVIEW_LINES as f32 * 18. + 16.))
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(th.line)
        .text_xs()
        .line_height(px(18.))
        .text_color(th.mute)
        .child(
            div()
                .line_clamp(PREVIEW_LINES)
                .child(comment(author, text, th)),
        )
}

/// "author: text" as one truncating line, with the author a little brighter.
fn comment(author: Option<String>, text: String, th: Theme) -> StyledText {
    let Some(author) = author else {
        return StyledText::new(text);
    };
    let full = format!("{author}: {text}");
    let highlight = HighlightStyle {
        color: Some(th.fg.into()),
        ..Default::default()
    };
    StyledText::new(full).with_highlights([(0..author.len() + 1, highlight)])
}

/// Linear-style: urgent is a filled square with "!", the rest are 1-3 signal bars.
fn priority_icon(p: u8, th: Theme) -> Div {
    let base = div()
        .size(px(14.))
        .flex_none()
        .flex()
        .items_end()
        .justify_center();
    match p {
        1 => base.child(
            div()
                .size(px(12.))
                .rounded(px(3.))
                .bg(rgb(0xf2994a))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(0xffffff))
                .text_size(px(10.))
                .font_weight(FontWeight::BOLD)
                .child("!"),
        ),
        _ => base
            .gap(px(1.5))
            .pb(px(1.))
            .children([4., 7., 10.].into_iter().enumerate().map(|(i, h)| {
                let on = p != 0 && i < (5 - p as usize);
                div()
                    .w(px(3.))
                    .h(px(h))
                    .rounded(px(1.))
                    .bg(if on { th.fg } else { th.line })
            })),
    }
}

fn status_icon(t: &Thread, th: Theme) -> (&'static str, Rgba) {
    let s = t.state.as_deref();
    match t.kind.as_str() {
        "PullRequest" => match s {
            Some("merged") => ("icons/git-merge.svg", th.purple),
            Some("closed") => ("icons/git-pull-request-closed.svg", th.red),
            Some("draft") => ("icons/git-pull-request-draft.svg", th.gray),
            Some(_) => ("icons/git-pull-request.svg", th.green),
            None => ("icons/git-pull-request.svg", th.gray),
        },
        "Issue" => match s {
            Some("closed") => ("icons/issue-closed.svg", th.purple),
            Some("not_planned") => ("icons/skip.svg", th.gray),
            Some(_) => ("icons/issue-opened.svg", th.green),
            None => ("icons/issue-opened.svg", th.gray),
        },
        "Release" => ("icons/tag.svg", th.mute),
        "Discussion" => ("icons/comment-discussion.svg", th.mute),
        "CheckSuite" => ("icons/workflow.svg", th.mute),
        "Commit" => ("icons/git-commit.svg", th.mute),
        k if k.contains("Alert") || k.contains("Advisory") => ("icons/shield.svg", th.red),
        _ => ("icons/bell.svg", th.mute),
    }
}

/// Stage names by thread type and whether it's in your repo or upstream.
fn stages(t: &Thread) -> [&'static str; 3] {
    match (t.kind.as_str(), t.mine) {
        ("PullRequest", true) => ["new", "reviewed", "merged"],
        ("PullRequest", false) => ["submitted", "feedback", "merged"],
        ("Issue", true) => ["new", "triaged", "closed"],
        ("Issue", false) => ["reported", "answered", "resolved"],
        _ => ["new", "seen", "done"],
    }
}

/// The last stage comes from GitHub's PR/issue state.
fn final_stage(t: &Thread) -> Option<&'static str> {
    match (t.kind.as_str(), t.state.as_deref()) {
        (_, Some("merged")) => Some("merged"),
        ("PullRequest", Some("closed")) => Some("closed"),
        (_, Some("not_planned")) => Some("not planned"),
        (_, Some("closed")) => Some(stages(t)[2]),
        _ => (t.stage == Some(2)).then(|| stages(t)[2]),
    }
}

fn progress(t: &Thread, th: Theme) -> Option<Div> {
    let stage = t.stage? as usize;
    let (stage, label, color) = match final_stage(t) {
        Some(label) => {
            let color = match (t.kind.as_str(), t.state.as_deref()) {
                ("PullRequest", Some("closed")) => th.red,
                (_, Some("merged" | "closed")) => th.purple,
                (_, Some("not_planned")) => th.gray,
                _ => th.green,
            };
            (2, label, color)
        }
        None => (stage.min(1), stages(t)[stage.min(1)], th.accent),
    };
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .children((0..3).map(|i| {
                div()
                    .w(px(10.))
                    .h(px(3.))
                    .rounded(px(2.))
                    .bg(if i <= stage { color } else { th.line })
            }))
            .child(
                div()
                    .w(px(84.))
                    .whitespace_nowrap()
                    .pl_1()
                    .text_xs()
                    .text_color(color)
                    .child(label),
            ),
    )
}
