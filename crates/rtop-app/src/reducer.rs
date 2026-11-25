// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The deterministic event reducer.

use rtop_actions::Outcome;
use rtop_collectors::inspect::ProcessDetail;
use rtop_core::{EdgeKind, EntityId, EntityKind, ProcessIdentity, SystemGraph, Timestamp};

use crate::filter::Filter;
use crate::input::{Hit, Key, KeyCode, MouseAction};
use crate::keymap::{Cmd, Resolution, resolve};
use crate::project::{ProjectionContext, project};
use crate::state::{AppState, Detail, Location, MenuAction, MenuItem, Modal, Mode, Target};
use crate::view::{Panel, RowId, SortKey, View};

#[derive(Debug, Clone)]
pub enum Event {
    Key(Key),
    Mouse(MouseAction),
    Resize {
        width: u16,
        height: u16,
    },
    /// The graph changed (patches applied by the owner).
    GraphChanged,
    /// Periodic wake-up (expiring notices).
    Tick,
    ActionDone {
        target: Target,
        signal_name: &'static str,
        outcome: Outcome,
    },
    Detail(Result<Box<ProcessDetail>, (ProcessIdentity, String)>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Deliver `signal` to each target, validating identity first.
    Signal {
        targets: Vec<Target>,
        signal: i32,
        signal_name: &'static str,
    },
    /// Start (or stop with `None`) deep inspection of a process.
    Inspect(Option<ProcessIdentity>),
    /// Expensive collection the current view needs.
    Subscribe {
        process_io: bool,
        socket_owners: bool,
        gpu_clients: bool,
    },
    Emergency(bool),
    Pause(bool),
    IntervalScale(f64),
    /// Put text on the clipboard (OSC 52).
    Copy(String),
    /// Stop rtop with SIGTSTP (Ctrl+Z).
    SuspendSelf,
    Quit,
}

/// Applies one event. `now` is when it happened.
pub fn reduce(s: &mut AppState, g: &SystemGraph, ev: Event, now: Timestamp, fx: &mut Vec<Effect>) {
    s.now = now;
    match ev {
        Event::Key(k) => {
            key(s, g, k, fx);
            s.dirty = true;
        }
        Event::Mouse(m) => {
            mouse(s, g, m, fx);
            s.dirty = true;
        }
        Event::Resize { width, height } => {
            s.width = width;
            s.height = height;
            s.dirty = true;
        }
        Event::GraphChanged => {
            refresh(s, g);
            record_history(s, g);
            if let Some(Detail::Ready(d)) = &s.detail {
                s.detail_age += 1;
                if s.detail_age >= 3 {
                    s.detail_age = 0;
                    fx.push(Effect::Inspect(Some(d.identity)));
                }
            }
            s.dirty = true;
        }
        Event::Tick => {
            if s.notices.back().is_some_and(|n| {
                now.saturating_since(n.at).as_secs() >= 6
                    && now.saturating_since(n.at).as_secs() < 8
            }) {
                s.dirty = true;
            }
        }
        Event::ActionDone {
            target,
            signal_name,
            outcome,
        } => {
            let text = format!(
                "{signal_name} → {} ({}): {}",
                target.name,
                target.identity.pid,
                outcome.describe()
            );
            s.notice(text, !outcome.is_success());
        }
        Event::Detail(r) => {
            let current = current_process(s);
            match r {
                Ok(d) if Some(d.identity) == current => s.detail = Some(Detail::Ready(d)),
                Err((id, e)) if Some(id) == current => s.detail = Some(Detail::Failed(id, e)),
                _ => {} // result for a process no longer inspected
            }
            s.dirty = true;
        }
    }
    sync_subscriptions(s, fx);
}

fn current_process(s: &AppState) -> Option<ProcessIdentity> {
    s.location
        .inspect
        .as_ref()
        .and_then(ProcessIdentity::from_entity_id)
}

// ----- rows and selection ------------------------------------------------

/// Recomputes every panel of the current location and repairs selections.
pub fn refresh(s: &mut AppState, g: &SystemGraph) {
    let ctx = ProjectionContext {
        tree: s.tree,
        collapsed: &s.collapsed,
        inspect: s.location.inspect.as_ref(),
    };
    s.rows = s
        .location
        .tables
        .iter()
        .map(|t| project(g, t, ctx))
        .collect();
    s.rows_generation = g.generation();
    for (i, t) in s.location.tables.iter_mut().enumerate() {
        let rows = &s.rows[i];
        // No selection is a valid state: the view then stays where it is
        // while rows re-sort. A selected row that disappears (its process
        // exited) hands the cursor to the row now at its position.
        match t.selected.as_ref().map(|r| rows.position(r)) {
            Some(Some(p)) => t.last_index = p,
            Some(None) => {
                t.selected =
                    (!rows.is_empty()).then(|| rows.ids[t.last_index.min(rows.len() - 1)].clone());
            }
            // Relationship lists do not re-sort under the user, so the
            // inspection page starts with its first relation selected.
            None if t.panel == Panel::Relations && !rows.is_empty() => {
                t.selected = Some(rows.ids[0].clone());
            }
            None => {}
        }
    }
    if !s.selection.is_empty() {
        let rows = &s.rows;
        s.selection
            .retain(|r| rows.iter().any(|x| x.position(r).is_some()));
    }
}

fn select_index(s: &mut AppState, panel_idx: usize, i: usize) {
    let rows = s.panel_rows(panel_idx);
    if rows.is_empty() {
        return;
    }
    let i = i.min(rows.len() - 1);
    let id = rows.ids[i].clone();
    let t = &mut s.location.tables[panel_idx];
    t.selected = Some(id);
    t.last_index = i;
}

fn move_cursor(s: &mut AppState, panel_idx: usize, delta: i64) {
    let rows = s.panel_rows(panel_idx);
    if rows.is_empty() {
        return;
    }
    let t = &s.location.tables[panel_idx];
    let next = match t.selected.as_ref().and_then(|r| rows.position(r)) {
        Some(cur) => (cur as i64 + delta).clamp(0, rows.len() as i64 - 1) as usize,
        // Nothing selected: the first motion selects the top visible row.
        None => t.offset.get().min(rows.len() - 1),
    };
    select_index(s, panel_idx, next);
}

fn focus_panel(s: &mut AppState, panel: Panel) -> Option<usize> {
    let i = s.location.tables.iter().position(|t| t.panel == panel)?;
    s.location.focus = i;
    Some(i)
}

fn select_row(s: &mut AppState, panel: Panel, row: &RowId) -> bool {
    let Some(i) = focus_panel(s, panel) else {
        return false;
    };
    match s.panel_rows(i).position(row) {
        Some(p) => {
            select_index(s, i, p);
            true
        }
        None => false,
    }
}

// ----- navigation --------------------------------------------------------

fn navigate(s: &mut AppState, g: &SystemGraph, to: Location, fx: &mut Vec<Effect>) {
    let from = std::mem::replace(&mut s.location, to);
    s.back.push(from);
    if s.back.len() > 100 {
        s.back.remove(0);
    }
    s.forward.clear();
    entered(s, g, fx);
}

fn entered(s: &mut AppState, g: &SystemGraph, fx: &mut Vec<Effect>) {
    s.mode = Mode::Normal;
    s.selection.clear();
    refresh(s, g);
    let p = current_process(s);
    let wanted = p.map(Detail::Loading);
    let changed = match (&s.detail, p) {
        (Some(Detail::Ready(d)), Some(id)) => d.identity != id,
        (Some(Detail::Loading(a) | Detail::Failed(a, _)), Some(id)) => *a != id,
        (None, None) => false,
        _ => true,
    };
    if changed {
        s.detail = wanted;
        s.detail_age = 0;
        fx.push(Effect::Inspect(p));
    }
}

fn switch_view(s: &mut AppState, g: &SystemGraph, v: View, fx: &mut Vec<Effect>) {
    if s.location.view == v && s.location.inspect.is_none() {
        return;
    }
    if s.location.inspect.is_none() {
        s.views.insert(s.location.view, s.location.clone());
    }
    let to = s
        .views
        .get(&v)
        .cloned()
        .unwrap_or_else(|| Location::for_view(v));
    navigate(s, g, to, fx);
}

fn back(s: &mut AppState, g: &SystemGraph, fx: &mut Vec<Effect>) -> bool {
    let Some(prev) = s.back.pop() else {
        return false;
    };
    let cur = std::mem::replace(&mut s.location, prev);
    s.forward.push(cur);
    entered(s, g, fx);
    true
}

fn forward(s: &mut AppState, g: &SystemGraph, fx: &mut Vec<Effect>) {
    if let Some(next) = s.forward.pop() {
        let cur = std::mem::replace(&mut s.location, next);
        s.back.push(cur);
        entered(s, g, fx);
    }
}

/// The entity a row leads to when opened.
fn row_target(s: &AppState, g: &SystemGraph, row: &RowId) -> Option<EntityId> {
    match row {
        RowId::Node(id) => Some(id.clone()),
        RowId::Edge(k) => match &s.location.inspect {
            Some(cur) if *cur == k.source => Some(k.target.clone()),
            Some(cur) if *cur == k.target => Some(k.source.clone()),
            _ => Some(k.source.clone()),
        },
        RowId::Seq(seq) => {
            let idx = seq.checked_sub(g.events_dropped())? as usize;
            g.events()
                .nth(idx)
                .and_then(|e| e.subjects.first().cloned())
        }
        RowId::Collector(_) => None,
    }
}

fn open(s: &mut AppState, g: &SystemGraph, fx: &mut Vec<Effect>) {
    let Some(row) = s.location.table().selected.clone() else {
        return;
    };
    match row_target(s, g, &row) {
        Some(id) if g.node(&id).is_some() || id.kind() == Some(EntityKind::Process) => {
            let view = s.location.view;
            navigate(s, g, Location::inspecting(view, id), fx);
        }
        Some(id) => s.notice(format!("{id} is not currently observed"), true),
        None => {}
    }
}

// ----- targets and actions -----------------------------------------------

fn target_of(g: &SystemGraph, id: &EntityId) -> Option<Target> {
    let identity = ProcessIdentity::from_entity_id(id)?;
    let name = g
        .node(id)
        .map(|n| n.label().to_owned())
        .unwrap_or_else(|| format!("pid {}", identity.pid));
    Some(Target { identity, name })
}

fn row_process(s: &AppState, row: &RowId) -> Option<EntityId> {
    match row {
        RowId::Node(id) if id.kind() == Some(EntityKind::Process) => Some(id.clone()),
        RowId::Edge(k)
            if matches!(
                k.kind,
                EdgeKind::CommunicatesWith
                    | EdgeKind::UsesGpu
                    | EdgeKind::OwnsSocket
                    | EdgeKind::ParentOf
                    | EdgeKind::MemberOf
            ) =>
        {
            let other = if s.location.inspect.as_ref() == Some(&k.source) {
                &k.target
            } else {
                &k.source
            };
            (other.kind() == Some(EntityKind::Process)).then(|| other.clone())
        }
        _ => None,
    }
}

/// Processes the next action applies to, captured now: the selection set,
/// else the visual range, else the cursor row, else the inspected process.
fn targets(s: &AppState, g: &SystemGraph) -> Vec<Target> {
    let mut rows: Vec<RowId> = s.selection.iter().cloned().collect();
    if let Mode::Visual { anchor } = &s.mode {
        let r = s.focused_rows();
        if let (Some(a), Some(c)) = (r.position(anchor), s.cursor()) {
            rows.extend(r.ids[a.min(c)..=a.max(c)].iter().cloned());
        }
    }
    if rows.is_empty()
        && let Some(r) = s.location.table().selected.clone()
        && (row_process(s, &r).is_some() || s.location.inspect.is_none())
    {
        rows.push(r);
    }
    let mut out: Vec<Target> = rows
        .iter()
        .filter_map(|r| row_process(s, r))
        .filter_map(|id| target_of(g, &id))
        .collect();
    if out.is_empty()
        && let Some(id) = s.location.inspect.clone()
    {
        out.extend(target_of(g, &id));
    }
    out.sort_by_key(|t| t.identity);
    out.dedup();
    out
}

fn act(
    s: &mut AppState,
    g: &SystemGraph,
    signal: i32,
    name: &'static str,
    confirm: bool,
    fx: &mut Vec<Effect>,
) {
    if !s.actions_enabled {
        s.notice(
            "Process actions are disabled (not observing the live system, or no pidfd support)",
            true,
        );
        return;
    }
    let t = targets(s, g);
    if t.is_empty() {
        s.notice("Select a process first", true);
        return;
    }
    if confirm || t.len() > 1 {
        s.modal = Some(Modal::Confirm {
            targets: t,
            signal,
            signal_name: name,
        });
    } else {
        send(s, t, signal, name, fx);
    }
}

fn send(
    s: &mut AppState,
    targets: Vec<Target>,
    signal: i32,
    name: &'static str,
    fx: &mut Vec<Effect>,
) {
    let who = match targets.as_slice() {
        [one] => format!("{} ({})", one.name, one.identity.pid),
        many => format!("{} processes", many.len()),
    };
    s.notice(format!("Sending {name} to {who}…"), false);
    s.mode = Mode::Normal;
    s.selection.clear();
    fx.push(Effect::Signal {
        targets,
        signal,
        signal_name: name,
    });
}

fn signal_list() -> Vec<rtop_linux::sys::SignalInfo> {
    rtop_linux::sys::signals()
}

pub fn filtered_signals(query: &str) -> Vec<rtop_linux::sys::SignalInfo> {
    let q = query.to_ascii_lowercase();
    signal_list()
        .into_iter()
        .filter(|s| {
            q.is_empty()
                || s.name.to_ascii_lowercase().contains(&q)
                || s.description.to_ascii_lowercase().contains(&q)
                || s.number.to_string() == q
        })
        .collect()
}

fn context_menu(s: &mut AppState, g: &SystemGraph, x: u16, y: u16) {
    let t = targets(s, g);
    let row = s.location.table().selected.clone();
    let title = match t.as_slice() {
        [one] => format!("{} · PID {}", one.name, one.identity.pid),
        [] => row
            .as_ref()
            .and_then(|r| row_target(s, g, r))
            .map(|id| crate::project::label_of(g, &id))
            .unwrap_or_else(|| "Actions".into()),
        many => format!("{} processes", many.len()),
    };
    let mut items = vec![
        MenuItem {
            label: "Inspect".into(),
            hint: "Enter",
            action: MenuAction::Inspect,
        },
        MenuItem {
            label: "Show relationships".into(),
            hint: "",
            action: MenuAction::ShowRelations,
        },
        MenuItem {
            label: "Copy information".into(),
            hint: "y",
            action: MenuAction::Yank,
        },
    ];
    if !t.is_empty() {
        items.extend([
            MenuItem {
                label: "Suspend (SIGSTOP)".into(),
                hint: "gp",
                action: MenuAction::Signal("SIGSTOP"),
            },
            MenuItem {
                label: "Resume (SIGCONT)".into(),
                hint: "gr",
                action: MenuAction::Signal("SIGCONT"),
            },
            MenuItem {
                label: "Terminate (SIGTERM)".into(),
                hint: "x",
                action: MenuAction::Signal("SIGTERM"),
            },
            MenuItem {
                label: "Send signal…".into(),
                hint: "gs",
                action: MenuAction::SignalMenu,
            },
            MenuItem {
                label: "Force kill (SIGKILL)".into(),
                hint: "X",
                action: MenuAction::Signal("SIGKILL"),
            },
        ]);
    }
    s.modal = Some(Modal::Context {
        targets: t,
        title,
        items,
        cursor: 0,
        x,
        y,
    });
}

/// Signal number by name from the architecture's signal table.
fn signal_number(name: &str) -> i32 {
    static TABLE: std::sync::OnceLock<Vec<rtop_linux::sys::SignalInfo>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(rtop_linux::sys::signals)
        .iter()
        .find(|s| s.name == name)
        .map_or(0, |s| s.number)
}

fn run_menu(
    s: &mut AppState,
    g: &SystemGraph,
    action: MenuAction,
    targets: Vec<Target>,
    fx: &mut Vec<Effect>,
) {
    s.modal = None;
    match action {
        MenuAction::Inspect | MenuAction::ShowRelations => open(s, g, fx),
        MenuAction::Yank => run(s, g, Cmd::Yank, fx),
        MenuAction::SignalMenu => {
            s.modal = Some(Modal::Signals {
                targets,
                query: String::new(),
                cursor: 0,
            })
        }
        MenuAction::Signal(name) => {
            if !s.actions_enabled {
                s.notice("Process actions are disabled (not observing the live system, or no pidfd support)", true);
            } else if name == "SIGTERM" || targets.len() > 1 {
                s.modal = Some(Modal::Confirm {
                    targets,
                    signal: signal_number(name),
                    signal_name: name,
                });
            } else {
                send(s, targets, signal_number(name), name, fx);
            }
        }
    }
}

// ----- commands ----------------------------------------------------------

fn sort(s: &mut AppState, g: &SystemGraph, key: SortKey, toggle_same: bool) {
    let panel = s.location.table().panel;
    if !panel.sort_keys().contains(&key) {
        s.notice(
            format!("{} cannot be sorted by {}", panel.title(), key.label()),
            true,
        );
        return;
    }
    let t = s.location.table_mut();
    if t.sort == key && toggle_same {
        t.descending = !t.descending;
    } else {
        t.sort = key;
        t.descending = !matches!(key, SortKey::Name | SortKey::Pid);
    }
    if key == SortKey::Pss {
        s.notice(
            "PSS is measured on inspection only; processes without it sort last",
            false,
        );
    }
    refresh(s, g);
}

fn summary(g: &SystemGraph, id: &EntityId) -> String {
    let Some(n) = g.node(id) else {
        return id.to_string();
    };
    let mut parts = vec![n.label().to_owned()];
    for k in ["pid", "user", "state"] {
        if let Some(a) = n.attr(k) {
            parts.push(format!(
                "{k}={}",
                match a {
                    rtop_core::Attr::Text(t) => t.clone(),
                    other => format!("{other:?}")
                        .trim_start_matches("UInt(")
                        .trim_start_matches("Int(")
                        .trim_end_matches(')')
                        .to_owned(),
                }
            ));
        }
    }
    if let Some(c) = n.text("cmdline").filter(|c| !c.is_empty()) {
        parts.push(format!("cmd={c}"));
    }
    parts.push(format!("id={id}"));
    parts.join(" ")
}

pub fn run(s: &mut AppState, g: &SystemGraph, cmd: Cmd, fx: &mut Vec<Effect>) {
    let f = s.location.focus;
    let page = s.location.table().viewport.get().max(1) as i64;
    match cmd {
        Cmd::View(v) => switch_view(s, g, v, fx),
        Cmd::Down => move_cursor(s, f, 1),
        Cmd::Up => move_cursor(s, f, -1),
        Cmd::FocusNext | Cmd::FocusPrev => {
            let n = s.location.tables.len();
            if n > 1 {
                s.location.focus = if cmd == Cmd::FocusNext {
                    (f + 1) % n
                } else {
                    (f + n - 1) % n
                };
            }
        }
        Cmd::Top => select_index(s, f, 0),
        Cmd::Bottom => select_index(s, f, usize::MAX),
        Cmd::HalfPageDown => move_cursor(s, f, page / 2),
        Cmd::HalfPageUp => move_cursor(s, f, -page / 2),
        Cmd::PageDown => move_cursor(s, f, page),
        Cmd::PageUp => move_cursor(s, f, -page),
        Cmd::ScreenTop | Cmd::ScreenMiddle | Cmd::ScreenBottom => {
            let off = s.location.table().offset.get();
            let vis = (s.focused_rows().len().saturating_sub(off))
                .min(page as usize)
                .max(1);
            let i = match cmd {
                Cmd::ScreenTop => off,
                Cmd::ScreenMiddle => off + vis / 2,
                _ => off + vis - 1,
            };
            select_index(s, f, i);
        }
        Cmd::NextColumn | Cmd::PrevColumn => {
            let keys = s.location.table().panel.sort_keys();
            let cur = keys
                .iter()
                .position(|k| *k == s.location.table().sort)
                .unwrap_or(0);
            let next = if cmd == Cmd::NextColumn {
                (cur + 1) % keys.len()
            } else {
                (cur + keys.len() - 1) % keys.len()
            };
            sort(s, g, keys[next], false);
        }
        Cmd::Open => open(s, g, fx),
        Cmd::Expand | Cmd::Collapse => {
            if s.location.inspect.is_some() {
                for sec in ["threads", "descriptors", "limits", "namespaces"] {
                    if cmd == Cmd::Expand {
                        s.folded.remove(sec)
                    } else {
                        s.folded.insert(sec)
                    };
                }
            } else if let Some(id) = s
                .location
                .table()
                .selected
                .as_ref()
                .and_then(RowId::entity)
                .cloned()
            {
                if cmd == Cmd::Collapse {
                    s.collapsed.insert(id)
                } else {
                    s.collapsed.remove(&id)
                };
                refresh(s, g);
            }
        }
        Cmd::ExpandAll => {
            s.collapsed.clear();
            s.folded.clear();
            refresh(s, g);
        }
        Cmd::CollapseAll => {
            if s.location.inspect.is_some() {
                s.folded
                    .extend(["threads", "descriptors", "limits", "namespaces"]);
            } else {
                let r = s.focused_rows().clone();
                for (i, id) in r.ids.iter().enumerate() {
                    if r.has_children[i]
                        && let Some(e) = id.entity()
                    {
                        s.collapsed.insert(e.clone());
                    }
                }
                refresh(s, g);
            }
        }
        Cmd::Tree => {
            s.tree = !s.tree;
            refresh(s, g);
            s.notice(
                if s.tree {
                    "Process tree on (zo/zc expand/collapse)"
                } else {
                    "Process tree off"
                },
                false,
            );
        }
        Cmd::SearchForward | Cmd::SearchBackward => {
            s.mode = Mode::Search {
                backward: cmd == Cmd::SearchBackward,
                input: String::new(),
            };
            s.prompt_error = None;
        }
        Cmd::NextMatch | Cmd::PrevMatch => match s.last_search.clone() {
            Some((filter, backward)) => search(s, g, &filter, backward ^ (cmd == Cmd::PrevMatch)),
            None => s.notice("No previous search", true),
        },
        Cmd::Filter => {
            let original = s.location.table().filter.source().to_owned();
            s.mode = Mode::Filter {
                input: original.clone(),
                original,
            };
            s.prompt_error = None;
        }
        Cmd::VisualMode => {
            s.mode = match (&s.mode, s.location.table().selected.clone()) {
                (Mode::Visual { .. }, _) | (_, None) => Mode::Normal,
                (_, Some(anchor)) => Mode::Visual { anchor },
            };
        }
        Cmd::ToggleSelect => {
            if let Some(r) = s.location.table().selected.clone() {
                if !s.selection.remove(&r) {
                    s.selection.insert(r);
                }
                move_cursor(s, f, 1);
            }
        }
        Cmd::Yank => {
            let text = match s
                .location
                .table()
                .selected
                .as_ref()
                .and_then(|r| row_target(s, g, r))
                .or_else(|| s.location.inspect.clone())
            {
                Some(id) => summary(g, &id),
                None => return,
            };
            fx.push(Effect::Copy(text));
            s.notice("Copied to clipboard", false);
        }
        Cmd::CommandLine => {
            s.mode = Mode::Command {
                input: String::new(),
            };
            s.prompt_error = None;
        }
        Cmd::Back => {
            if !back(s, g, fx) {
                s.notice("Already at the oldest location", false);
            }
        }
        Cmd::Forward => forward(s, g, fx),
        Cmd::Cancel => {
            if !s.pending_keys.is_empty()
                || !matches!(s.mode, Mode::Normal)
                || !s.selection.is_empty()
            {
                s.mode = Mode::Normal;
                s.selection.clear();
            } else if s.location.inspect.is_some() {
                back(s, g, fx);
            } else {
                // Deselect: the view stays exactly where it is and stops
                // following a row as the table re-sorts.
                s.location.table_mut().selected = None;
            }
        }
        Cmd::Quit => {
            if s.location.inspect.is_some() && back(s, g, fx) {
            } else {
                s.quit = true;
                fx.push(Effect::Quit);
            }
        }
        Cmd::ForceQuit => {
            s.quit = true;
            fx.push(Effect::Quit);
        }
        Cmd::Sort(k) => sort(s, g, k, false),
        Cmd::SortReverse => {
            let t = s.location.table_mut();
            t.descending = !t.descending;
            refresh(s, g);
        }
        Cmd::SortMenu => s.modal = Some(Modal::SortMenu { cursor: 0 }),
        Cmd::Terminate => act(s, g, signal_number("SIGTERM"), "SIGTERM", true, fx),
        Cmd::ForceKill => act(s, g, signal_number("SIGKILL"), "SIGKILL", false, fx),
        Cmd::Suspend => act(s, g, signal_number("SIGSTOP"), "SIGSTOP", false, fx),
        Cmd::Resume => act(s, g, signal_number("SIGCONT"), "SIGCONT", false, fx),
        Cmd::SignalMenu => {
            let t = targets(s, g);
            if t.is_empty() {
                s.notice("Select a process first", true);
            } else {
                s.modal = Some(Modal::Signals {
                    targets: t,
                    query: String::new(),
                    cursor: 0,
                });
            }
        }
        Cmd::ContextMenu => context_menu(s, g, s.width / 3, s.height / 3),
        Cmd::Emergency => {
            s.emergency = !s.emergency;
            fx.push(Effect::Emergency(s.emergency));
            s.notice(
                if s.emergency {
                    "EMERGENCY MODE: only CPU, memory, pressure and processes are collected"
                } else {
                    "Emergency mode off: full monitoring restored"
                },
                s.emergency,
            );
        }
        Cmd::Pause => {
            s.paused = !s.paused;
            fx.push(Effect::Pause(s.paused));
            s.notice(
                if s.paused {
                    "Updates paused (p to resume)"
                } else {
                    "Updates resumed"
                },
                false,
            );
        }
        Cmd::Faster | Cmd::Slower => {
            s.interval_scale = if cmd == Cmd::Faster {
                (s.interval_scale / 2.0).max(0.25)
            } else {
                (s.interval_scale * 2.0).min(16.0)
            };
            fx.push(Effect::IntervalScale(s.interval_scale));
            s.notice(format!("Refresh interval ×{}", s.interval_scale), false);
        }
        Cmd::Help => s.modal = Some(Modal::Help { scroll: 0 }),
        Cmd::SuspendRtop => fx.push(Effect::SuspendSelf),
    }
}

fn search(s: &mut AppState, g: &SystemGraph, filter: &Filter, backward: bool) {
    let rows = s.focused_rows().clone();
    if rows.is_empty() {
        return;
    }
    let n = rows.len();
    let start = s.cursor().unwrap_or(0);
    for step in 1..=n {
        let i = if backward {
            (start + n * 2 - step) % n
        } else {
            (start + step) % n
        };
        let hit = rows.ids[i]
            .entity()
            .and_then(|id| g.node(id))
            .is_some_and(|node| filter.matches(node));
        if hit {
            let wrapped = if backward { i > start } else { i <= start };
            select_index(s, s.location.focus, i);
            if wrapped {
                s.notice(
                    if backward {
                        "search hit TOP, continuing at BOTTOM"
                    } else {
                        "search hit BOTTOM, continuing at TOP"
                    },
                    false,
                );
            }
            return;
        }
    }
    s.notice(format!("Pattern not found: {}", filter.source()), true);
}

fn command_line(s: &mut AppState, g: &SystemGraph, line: &str, fx: &mut Vec<Effect>) {
    let mut parts = line.trim().splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    match cmd {
        "" => {}
        "q" | "quit" | "q!" | "qa" => run(s, g, Cmd::ForceQuit, fx),
        "h" | "help" => run(s, g, Cmd::Help, fx),
        "back" => run(s, g, Cmd::Back, fx),
        "forward" => run(s, g, Cmd::Forward, fx),
        "tree" => run(s, g, Cmd::Tree, fx),
        "emergency" => run(s, g, Cmd::Emergency, fx),
        "pause" | "resume" => run(s, g, Cmd::Pause, fx),
        "sort" => {
            let mut a = arg.split_whitespace();
            match a.next().and_then(SortKey::parse) {
                Some(k) => {
                    sort(s, g, k, false);
                    match a.next() {
                        Some("asc") => s.location.table_mut().descending = false,
                        Some("desc") => s.location.table_mut().descending = true,
                        _ => {}
                    }
                    refresh(s, g);
                }
                None => s.notice(
                    "Usage: :sort cpu|mem|pss|virt|io|net|gpu|threads|age|pid|name [asc|desc]",
                    true,
                ),
            }
        }
        "filter" | "f" => match Filter::parse(arg) {
            Ok(f) => {
                s.location.table_mut().filter = f;
                refresh(s, g);
            }
            Err(e) => s.notice(e, true),
        },
        "nofilter" | "nof" => {
            s.location.table_mut().filter = Filter::default();
            refresh(s, g);
        }
        "view" | "v" => match View::parse(arg) {
            Some(v) => switch_view(s, g, v, fx),
            None => s.notice(format!("Unknown view '{arg}'"), true),
        },
        "interval" => match arg.parse::<f64>() {
            Ok(x) if (0.25..=16.0).contains(&x) => {
                s.interval_scale = x;
                fx.push(Effect::IntervalScale(x));
            }
            _ => s.notice("Usage: :interval <factor 0.25–16>", true),
        },
        "signal" | "kill" | "sig" => {
            let name = arg.to_ascii_uppercase();
            let name = if name.starts_with("SIG") || name.parse::<i32>().is_ok() {
                name
            } else {
                format!("SIG{name}")
            };
            match signal_list()
                .into_iter()
                .find(|x| x.name == name || x.number.to_string() == name)
            {
                Some(sig) => act(
                    s,
                    g,
                    sig.number,
                    sig.name,
                    sig.name == "SIGTERM" || sig.name == "SIGKILL",
                    fx,
                ),
                None if arg.is_empty() => run(s, g, Cmd::SignalMenu, fx),
                None => s.notice(format!("Unknown signal '{arg}'"), true),
            }
        }
        other => match other.parse::<u32>().ok().and_then(View::from_digit) {
            Some(v) => switch_view(s, g, v, fx),
            None => s.notice(
                format!("Not an editor command: {other} (:help lists commands)"),
                true,
            ),
        },
    }
}

// ----- key and mouse dispatch -------------------------------------------

fn text_edit(input: &mut String, k: Key) -> bool {
    match k.code {
        KeyCode::Char('u') if k.ctrl => input.clear(),
        KeyCode::Char('w') if k.ctrl => {
            let t = input.trim_end().rfind(' ').map_or(0, |i| i + 1);
            input.truncate(t);
        }
        KeyCode::Char(c) if !k.ctrl => input.push(c),
        KeyCode::Backspace => {
            input.pop();
        }
        _ => return false,
    }
    true
}

fn key(s: &mut AppState, g: &SystemGraph, k: Key, fx: &mut Vec<Effect>) {
    if let Some(modal) = s.modal.take() {
        modal_key(s, g, modal, k, fx);
        return;
    }
    match std::mem::replace(&mut s.mode, Mode::Normal) {
        Mode::Search {
            backward,
            mut input,
        } => match k.code {
            KeyCode::Esc => {}
            KeyCode::Enter => match Filter::parse(&input) {
                Ok(f) if !f.is_empty() => {
                    s.last_search = Some((f.clone(), backward));
                    search(s, g, &f, backward);
                }
                Ok(_) => {}
                Err(e) => s.notice(e, true),
            },
            _ => {
                text_edit(&mut input, k);
                s.prompt_error = Filter::parse(&input).err();
                s.mode = Mode::Search { backward, input };
            }
        },
        Mode::Filter {
            mut input,
            original,
        } => match k.code {
            KeyCode::Esc => {
                s.location.table_mut().filter = Filter::parse(&original).unwrap_or_default();
                s.prompt_error = None;
                refresh(s, g);
            }
            KeyCode::Enter => s.prompt_error = None,
            _ => {
                text_edit(&mut input, k);
                // Parsed once per edit; rows recomputed only when valid.
                match Filter::parse(&input) {
                    Ok(f) => {
                        s.prompt_error = None;
                        s.location.table_mut().filter = f;
                        refresh(s, g);
                    }
                    Err(e) => s.prompt_error = Some(e),
                }
                s.mode = Mode::Filter { input, original };
            }
        },
        Mode::Command { mut input } => match k.code {
            KeyCode::Esc => {}
            KeyCode::Enter => command_line(s, g, &input, fx),
            KeyCode::Backspace if input.is_empty() => {}
            _ => {
                text_edit(&mut input, k);
                s.mode = Mode::Command { input };
            }
        },
        mode @ (Mode::Normal | Mode::Visual { .. }) => {
            s.mode = mode;
            if k.code == KeyCode::Esc && !s.pending_keys.is_empty() {
                s.pending_keys.clear();
                return;
            }
            match resolve(&s.pending_keys, k) {
                Resolution::Pending => s.pending_keys.push(k),
                Resolution::Cmd(c) => {
                    s.pending_keys.clear();
                    run(s, g, c, fx);
                }
                Resolution::Unbound => s.pending_keys.clear(),
            }
        }
    }
}

fn modal_key(s: &mut AppState, g: &SystemGraph, modal: Modal, k: Key, fx: &mut Vec<Effect>) {
    let down = matches!(k.code, KeyCode::Down | KeyCode::Char('j')) && !k.ctrl;
    let up = matches!(k.code, KeyCode::Up | KeyCode::Char('k')) && !k.ctrl;
    match modal {
        Modal::Help { scroll } => match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) => {}
            _ if down => s.modal = Some(Modal::Help { scroll: scroll + 1 }),
            _ if up => {
                s.modal = Some(Modal::Help {
                    scroll: scroll.saturating_sub(1),
                })
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                s.modal = Some(Modal::Help {
                    scroll: scroll + 10,
                })
            }
            KeyCode::PageUp => {
                s.modal = Some(Modal::Help {
                    scroll: scroll.saturating_sub(10),
                })
            }
            _ => s.modal = Some(Modal::Help { scroll }),
        },
        Modal::SortMenu { cursor } => {
            let keys = s.location.table().panel.sort_keys();
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Enter => sort(s, g, keys[cursor.min(keys.len() - 1)], true),
                _ if down => {
                    s.modal = Some(Modal::SortMenu {
                        cursor: (cursor + 1).min(keys.len() - 1),
                    })
                }
                _ if up => {
                    s.modal = Some(Modal::SortMenu {
                        cursor: cursor.saturating_sub(1),
                    })
                }
                _ => s.modal = Some(Modal::SortMenu { cursor }),
            }
        }
        Modal::Confirm {
            targets,
            signal,
            signal_name,
        } => match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                send(s, targets, signal, signal_name, fx)
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {
                s.notice(format!("{signal_name} cancelled; nothing was sent"), false)
            }
            _ => {
                s.modal = Some(Modal::Confirm {
                    targets,
                    signal,
                    signal_name,
                })
            }
        },
        Modal::Signals {
            targets,
            mut query,
            cursor,
        } => {
            let list = filtered_signals(&query);
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match list.get(cursor) {
                    Some(sig) if targets.len() > 1 => {
                        s.modal = Some(Modal::Confirm {
                            targets,
                            signal: sig.number,
                            signal_name: sig.name,
                        })
                    }
                    Some(sig) => send(s, targets, sig.number, sig.name, fx),
                    None => {
                        s.modal = Some(Modal::Signals {
                            targets,
                            query,
                            cursor,
                        })
                    }
                },
                KeyCode::Down => {
                    s.modal = Some(Modal::Signals {
                        targets,
                        query,
                        cursor: (cursor + 1).min(list.len().saturating_sub(1)),
                    })
                }
                KeyCode::Up => {
                    s.modal = Some(Modal::Signals {
                        targets,
                        query,
                        cursor: cursor.saturating_sub(1),
                    })
                }
                _ => {
                    text_edit(&mut query, k);
                    s.modal = Some(Modal::Signals {
                        targets,
                        query,
                        cursor: 0,
                    });
                }
            }
        }
        Modal::Context {
            targets,
            title,
            items,
            cursor,
            x,
            y,
        } => match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Enter => {
                let a = items[cursor.min(items.len() - 1)].action.clone();
                run_menu(s, g, a, targets, fx);
            }
            _ if down => {
                s.modal = Some(Modal::Context {
                    targets,
                    title,
                    cursor: (cursor + 1).min(items.len() - 1),
                    items,
                    x,
                    y,
                })
            }
            _ if up => {
                s.modal = Some(Modal::Context {
                    targets,
                    title,
                    cursor: cursor.saturating_sub(1),
                    items,
                    x,
                    y,
                })
            }
            _ => {
                s.modal = Some(Modal::Context {
                    targets,
                    title,
                    items,
                    cursor,
                    x,
                    y,
                })
            }
        },
    }
}

fn mouse(s: &mut AppState, g: &SystemGraph, m: MouseAction, fx: &mut Vec<Effect>) {
    if let Some(modal) = s.modal.take() {
        match (modal, m) {
            (Modal::Context { targets, items, .. }, MouseAction::Click(Hit::MenuItem(i)))
                if i < items.len() =>
            {
                run_menu(s, g, items[i].action.clone(), targets, fx)
            }
            (Modal::Signals { targets, query, .. }, MouseAction::Click(Hit::MenuItem(i))) => {
                if let Some(sig) = filtered_signals(&query).get(i) {
                    if targets.len() > 1 {
                        s.modal = Some(Modal::Confirm {
                            targets,
                            signal: sig.number,
                            signal_name: sig.name,
                        });
                    } else {
                        send(s, targets, sig.number, sig.name, fx);
                    }
                }
            }
            (Modal::SortMenu { .. }, MouseAction::Click(Hit::MenuItem(i))) => {
                if let Some(k) = s.location.table().panel.sort_keys().get(i).copied() {
                    sort(s, g, k, true);
                }
            }
            (Modal::Help { scroll }, MouseAction::Scroll { lines, .. }) => {
                s.modal = Some(Modal::Help {
                    scroll: (scroll as i64 + i64::from(lines)).max(0) as usize,
                })
            }
            (
                Modal::Confirm {
                    targets,
                    signal,
                    signal_name,
                },
                MouseAction::Click(Hit::MenuItem(0)),
            ) => send(s, targets, signal, signal_name, fx),
            (
                Modal::Confirm { signal_name, .. },
                MouseAction::Click(Hit::MenuItem(_)) | MouseAction::Dismiss,
            ) => s.notice(format!("{signal_name} cancelled; nothing was sent"), false),
            (_, MouseAction::Dismiss | MouseAction::Click(_) | MouseAction::RightClick { .. }) => {}
            (modal, _) => s.modal = Some(modal),
        }
        return;
    }
    match m {
        MouseAction::Click(Hit::Tab(v)) => switch_view(s, g, v, fx),
        MouseAction::Click(Hit::Back) => {
            back(s, g, fx);
        }
        MouseAction::Click(Hit::Row { panel, row }) => {
            select_row(s, panel, &row);
        }
        MouseAction::DoubleClick(Hit::Row { panel, row }) => {
            if select_row(s, panel, &row) {
                open(s, g, fx);
            }
        }
        MouseAction::RightClick {
            hit: Hit::Row { panel, row },
            x,
            y,
        } => {
            if select_row(s, panel, &row) {
                context_menu(s, g, x, y);
            }
        }
        MouseAction::ToggleClick(Hit::Row { panel, row }) => {
            if select_row(s, panel, &row) && !s.selection.remove(&row) {
                s.selection.insert(row);
            }
        }
        MouseAction::RangeClick(Hit::Row { panel, row }) => {
            let from = s.cursor();
            if select_row(s, panel, &row)
                && let (Some(a), Some(b)) = (from, s.cursor())
            {
                let ids = s.focused_rows().ids[a.min(b)..=a.max(b)].to_vec();
                s.selection.extend(ids);
            }
        }
        MouseAction::Click(Hit::Header { panel, sort: key })
        | MouseAction::DoubleClick(Hit::Header { panel, sort: key }) => {
            if focus_panel(s, panel).is_some() {
                sort(s, g, key, true);
            }
        }
        MouseAction::Click(Hit::Panel(p)) => {
            focus_panel(s, p);
        }
        MouseAction::Scroll { panel, lines } => {
            let idx = panel
                .and_then(|p| s.location.tables.iter().position(|t| t.panel == p))
                .unwrap_or(s.location.focus);
            if s.location.tables[idx].selected.is_some() {
                move_cursor(s, idx, i64::from(lines));
            } else {
                // Without a selection the wheel scrolls the view itself.
                let len = s.panel_rows(idx).len();
                let t = &s.location.tables[idx];
                let max = len.saturating_sub(t.viewport.get().max(1));
                t.offset
                    .set((t.offset.get() as i64 + i64::from(lines)).clamp(0, max as i64) as usize);
            }
        }
        _ => {}
    }
}

// ----- subscriptions and history -----------------------------------------

fn sync_subscriptions(s: &mut AppState, fx: &mut Vec<Effect>) {
    let v = s.location.view;
    let sort = s.location.tables.iter().map(|t| t.sort).collect::<Vec<_>>();
    let inspecting_process = current_process(s).is_some();
    let want = (
        matches!(v, View::Processes | View::Storage | View::Overview)
            || sort.contains(&SortKey::Io)
            || inspecting_process,
        matches!(v, View::Network | View::Containers)
            || sort.contains(&SortKey::Net)
            || inspecting_process,
        matches!(v, View::Gpu) || sort.contains(&SortKey::Gpu) || inspecting_process,
    );
    if want != s.subscriptions {
        s.subscriptions = want;
        fx.push(Effect::Subscribe {
            process_io: want.0,
            socket_owners: want.1,
            gpu_clients: want.2,
        });
    }
}

/// Host metrics kept as history for graphs.
pub const HOST_SERIES: &[&str] = &[
    "cpu_usage_percent",
    "memory_used_percent",
    "swap_used_bytes",
    "network_rx_bytes_per_second",
    "network_tx_bytes_per_second",
    "disk_read_bytes_per_second",
    "disk_write_bytes_per_second",
    "psi_cpu_some_avg10",
    "psi_memory_some_avg10",
    "psi_io_some_avg10",
    "temperature_max_celsius",
    "swap_in_pages_per_second",
];

fn record_history(s: &mut AppState, g: &SystemGraph) {
    let now = s.now;
    // One history sample per second, however often the graph changes.
    if s.last_history
        .is_some_and(|t| now.saturating_since(t) < std::time::Duration::from_millis(950))
    {
        return;
    }
    s.last_history = Some(now);
    let host = EntityId::host();
    if let Some(h) = g.node(&host) {
        for k in HOST_SERIES {
            s.history.push((host.clone(), k), h.f64(k), now);
        }
    }
    for c in g.nodes_of(EntityKind::LogicalCpu) {
        s.history.push(
            (c.id.clone(), "cpu_usage_percent"),
            c.f64("cpu_usage_percent"),
            now,
        );
    }
    for kind in [
        EntityKind::Gpu,
        EntityKind::Interface,
        EntityKind::BlockDevice,
    ] {
        for n in g.nodes_of(kind) {
            for k in [
                "gpu_usage_percent",
                "rx_bytes_per_second",
                "tx_bytes_per_second",
                "read_bytes_per_second",
                "write_bytes_per_second",
            ] {
                if n.metrics.contains_key(k) {
                    s.history.push((n.id.clone(), k), n.f64(k), now);
                }
            }
        }
    }
    let inspected = s.location.inspect.clone();
    if let Some(id) = &inspected
        && let Some(n) = g.node(id)
    {
        for k in n.metrics.keys().take(64) {
            s.history.push((id.clone(), k), n.f64(k), now);
        }
    }
    // History never outlives its entity, and per-metric series exist only
    // for the entity being inspected: memory stays proportional to the
    // hardware, not to how long rtop has run.
    s.history.retain(|(id, key)| {
        if Some(id) == inspected.as_ref() {
            return true;
        }
        match id.kind() {
            Some(EntityKind::Host) => HOST_SERIES.contains(key),
            Some(
                EntityKind::LogicalCpu
                | EntityKind::Gpu
                | EntityKind::Interface
                | EntityKind::BlockDevice,
            ) => g.node(id).is_some(),
            _ => false,
        }
    });
}
