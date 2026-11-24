// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Explicit application state. Orthogonal concerns (navigation, input mode,
//! modal, pending actions) are separate fields rather than one product
//! state, so their combinations need no enumeration.

use std::cell::Cell;
use std::collections::{BTreeSet, HashSet, VecDeque};

use rtop_collectors::inspect::ProcessDetail;
use rtop_core::history::HistoryStore;
use rtop_core::{EntityId, ProcessIdentity, Timestamp};

use crate::filter::Filter;
use crate::input::Key;
use crate::project::Rows;
use crate::view::{Panel, RowId, SortKey, View};

/// Per-table navigation state.
#[derive(Debug, Clone)]
pub struct TableState {
    pub panel: Panel,
    pub selected: Option<RowId>,
    pub sort: SortKey,
    pub descending: bool,
    pub filter: Filter,
    /// First visible row; adjusted by the renderer to keep the cursor visible.
    pub offset: Cell<usize>,
    /// Rows the renderer last had room for (for H/M/L and page moves).
    pub viewport: Cell<usize>,
    /// Cursor index last time a row was selected; used to keep the cursor in
    /// place when the selected entity disappears.
    pub last_index: usize,
}

impl TableState {
    pub fn new(panel: Panel, sort: SortKey) -> Self {
        let descending = sort != SortKey::Name && sort != SortKey::Pid;
        TableState {
            panel,
            selected: None,
            sort,
            descending,
            filter: Filter::default(),
            offset: Cell::new(0),
            viewport: Cell::new(20),
            last_index: 0,
        }
    }
}

/// One place in the navigation history.
#[derive(Debug, Clone)]
pub struct Location {
    pub view: View,
    /// The entity being inspected (level 2/3), if any.
    pub inspect: Option<EntityId>,
    pub tables: Vec<TableState>,
    pub focus: usize,
}

impl Location {
    pub fn for_view(view: View) -> Self {
        let tables = view
            .panels()
            .iter()
            .map(|p| {
                TableState::new(
                    *p,
                    if *p == Panel::Processes {
                        view.default_sort()
                    } else {
                        p.default_sort()
                    },
                )
            })
            .collect();
        Location {
            view,
            inspect: None,
            tables,
            focus: 0,
        }
    }

    pub fn inspecting(view: View, id: EntityId) -> Self {
        Location {
            view,
            inspect: Some(id),
            tables: vec![TableState::new(Panel::Relations, SortKey::Name)],
            focus: 0,
        }
    }

    pub fn table(&self) -> &TableState {
        &self.tables[self.focus.min(self.tables.len() - 1)]
    }

    pub fn table_mut(&mut self) -> &mut TableState {
        let i = self.focus.min(self.tables.len() - 1);
        &mut self.tables[i]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search {
        backward: bool,
        input: String,
    },
    Filter {
        input: String,
        original: String,
    },
    Command {
        input: String,
    },
    /// Visual range selection anchored at a row.
    Visual {
        anchor: RowId,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub hint: &'static str,
    pub action: MenuAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    Inspect,
    Signal(&'static str),
    SignalMenu,
    Yank,
    ShowRelations,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Modal {
    Help {
        scroll: usize,
    },
    SortMenu {
        cursor: usize,
    },
    /// Pick any signal for `targets`.
    Signals {
        targets: Vec<Target>,
        query: String,
        cursor: usize,
    },
    Confirm {
        targets: Vec<Target>,
        signal: i32,
        signal_name: &'static str,
    },
    Context {
        targets: Vec<Target>,
        title: String,
        items: Vec<MenuItem>,
        cursor: usize,
        x: u16,
        y: u16,
    },
}

/// A process an action will be applied to: identity captured when the user
/// chose it, plus the name shown at that time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub identity: ProcessIdentity,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub at: Timestamp,
    pub text: String,
    pub error: bool,
}

/// Process detail (level 3) for the inspected process.
#[derive(Debug, Clone)]
pub enum Detail {
    Loading(ProcessIdentity),
    Ready(Box<ProcessDetail>),
    Failed(ProcessIdentity, String),
}

#[derive(Debug)]
pub struct AppState {
    pub location: Location,
    pub back: Vec<Location>,
    pub forward: Vec<Location>,
    pub mode: Mode,
    pub modal: Option<Modal>,
    pub pending_keys: Vec<Key>,
    /// Rows of each panel of the current location.
    pub rows: Vec<Rows>,
    pub rows_generation: u64,
    pub selection: BTreeSet<RowId>,
    pub tree: bool,
    pub collapsed: HashSet<EntityId>,
    /// Collapsed sections of the inspection page.
    pub folded: HashSet<&'static str>,
    pub last_search: Option<(Filter, bool)>,
    pub notices: VecDeque<Notice>,
    pub detail: Option<Detail>,
    pub history: HistoryStore<(EntityId, &'static str)>,
    pub emergency: bool,
    pub paused: bool,
    pub interval_scale: f64,
    /// Whether process actions are possible (live system with pidfd).
    pub actions_enabled: bool,
    pub quit: bool,
    pub dirty: bool,
    pub width: u16,
    pub height: u16,
    pub now: Timestamp,
    pub detail_age: u32,
    /// Remembered location (sort, filter, selection) of every view.
    pub views: std::collections::HashMap<View, Location>,
    /// Subscriptions last requested: (process I/O, socket owners, GPU clients).
    pub subscriptions: (bool, bool, bool),
    /// Error of the expression being typed in a prompt.
    pub prompt_error: Option<String>,
}

impl AppState {
    pub fn new(actions_enabled: bool) -> Self {
        AppState {
            location: Location::for_view(View::Overview),
            back: Vec::new(),
            forward: Vec::new(),
            mode: Mode::Normal,
            modal: None,
            pending_keys: Vec::new(),
            rows: Vec::new(),
            rows_generation: u64::MAX,
            selection: BTreeSet::new(),
            tree: false,
            collapsed: HashSet::new(),
            folded: HashSet::new(),
            last_search: None,
            notices: VecDeque::new(),
            detail: None,
            history: HistoryStore::new(1024, 240, 10, 360),
            emergency: false,
            paused: false,
            interval_scale: 1.0,
            actions_enabled,
            quit: false,
            dirty: true,
            width: 120,
            height: 40,
            now: Timestamp::ZERO,
            detail_age: 0,
            views: std::collections::HashMap::new(),
            subscriptions: (false, false, false),
            prompt_error: None,
        }
    }

    pub fn panel_rows(&self, i: usize) -> &Rows {
        static EMPTY: std::sync::OnceLock<Rows> = std::sync::OnceLock::new();
        self.rows
            .get(i)
            .unwrap_or_else(|| EMPTY.get_or_init(Rows::default))
    }

    pub fn focused_rows(&self) -> &Rows {
        self.panel_rows(self.location.focus)
    }

    /// Index of the selected row in the focused table.
    pub fn cursor(&self) -> Option<usize> {
        let sel = self.location.table().selected.as_ref()?;
        self.focused_rows().position(sel)
    }

    pub fn notice(&mut self, text: impl Into<String>, error: bool) {
        if self.notices.len() >= 32 {
            self.notices.pop_front();
        }
        self.notices.push_back(Notice {
            at: self.now,
            text: text.into(),
            error,
        });
        self.dirty = true;
    }

    /// The latest notice if it is still recent enough to show.
    pub fn current_notice(&self) -> Option<&Notice> {
        self.notices
            .back()
            .filter(|n| self.now.saturating_since(n.at).as_secs() < 6)
    }
}
