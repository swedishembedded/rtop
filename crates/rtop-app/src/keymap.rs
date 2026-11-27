// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The single registry of commands and their key bindings.
//!
//! The reducer resolves keys through [`BINDINGS`] and the help screen is
//! generated from it, so documentation and behaviour cannot diverge. Mouse
//! input produces the same [`Cmd`]s.

use crate::input::{Key, KeyCode};
use crate::view::{SortKey, View};

/// A semantic command, independent of how it was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    View(View),
    Down,
    Up,
    FocusPrev,
    FocusNext,
    Top,
    Bottom,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    NextColumn,
    PrevColumn,
    ScreenTop,
    ScreenMiddle,
    ScreenBottom,
    Open,
    Expand,
    Collapse,
    ExpandAll,
    CollapseAll,
    SearchForward,
    SearchBackward,
    NextMatch,
    PrevMatch,
    Filter,
    VisualMode,
    ToggleSelect,
    Yank,
    CommandLine,
    Back,
    Forward,
    Cancel,
    Quit,
    ForceQuit,
    Sort(SortKey),
    SortReverse,
    SortMenu,
    Terminate,
    ForceKill,
    SignalMenu,
    Suspend,
    Resume,
    ContextMenu,
    Tree,
    Emergency,
    Pause,
    Faster,
    Slower,
    Help,
    SuspendRtop,
}

#[derive(Debug, Clone, Copy)]
pub struct Binding {
    pub keys: &'static [Key],
    pub cmd: Cmd,
    pub help: &'static str,
    pub group: &'static str,
}

macro_rules! k {
    ($c:literal) => {
        Key::ch($c)
    };
    (^$c:literal) => {
        Key::ctrl($c)
    };
    ($code:ident) => {
        Key::plain(KeyCode::$code)
    };
    (F $n:literal) => {
        Key::plain(KeyCode::F($n))
    };
}

macro_rules! bindings {
    ($( [$($key:tt)+] => $cmd:expr, $group:literal, $help:literal; )+) => {
        /// Every key binding, in help-screen order.
        pub const BINDINGS: &[Binding] = &[$( Binding { keys: &bindings!(@keys $($key)+), cmd: $cmd, help: $help, group: $group } ),+];
    };
    (@keys $($k:tt),+) => { [$(bindings!(@key $k)),+] };
    (@key $k:literal) => { Key::ch($k) };
    (@key $k:ident) => { k!($k) };
    (@key ($($k:tt)+)) => { k!($($k)+) };
}

bindings! {
    ['0'] => Cmd::View(View::Overview), "Views", "System overview";
    ['1'] => Cmd::View(View::Processes), "Views", "Processes";
    ['2'] => Cmd::View(View::Cpu), "Views", "CPU";
    ['3'] => Cmd::View(View::Memory), "Views", "Memory";
    ['4'] => Cmd::View(View::Storage), "Views", "Storage and filesystems";
    ['5'] => Cmd::View(View::Network), "Views", "Networking and sockets";
    ['6'] => Cmd::View(View::Containers), "Views", "Containers and services";
    ['7'] => Cmd::View(View::Gpu), "Views", "GPUs and accelerators";
    ['8'] => Cmd::View(View::Security), "Views", "Security, events and telemetry health";
    ['9'] => Cmd::View(View::Sensors), "Views", "Sensors and power";
    ['c'] => Cmd::View(View::Nodes), "Views", "Nodes of the Slurm job (inside a Slurm job)";
    ['j'] => Cmd::Down, "Move", "Down";
    [Down] => Cmd::Down, "Move", "Down";
    ['k'] => Cmd::Up, "Move", "Up";
    [Up] => Cmd::Up, "Move", "Up";
    ['h'] => Cmd::FocusPrev, "Move", "Previous panel";
    [Left] => Cmd::FocusPrev, "Move", "Previous panel";
    ['l'] => Cmd::FocusNext, "Move", "Next panel";
    [Right] => Cmd::FocusNext, "Move", "Next panel";
    [Tab] => Cmd::FocusNext, "Move", "Next panel";
    [BackTab] => Cmd::FocusPrev, "Move", "Previous panel";
    ['g', 'g'] => Cmd::Top, "Move", "First item";
    [Home] => Cmd::Top, "Move", "First item";
    ['G'] => Cmd::Bottom, "Move", "Last item";
    [End] => Cmd::Bottom, "Move", "Last item";
    [(^'d')] => Cmd::HalfPageDown, "Move", "Half page down";
    [(^'u')] => Cmd::HalfPageUp, "Move", "Half page up";
    [(^'f')] => Cmd::PageDown, "Move", "Page down";
    [PageDown] => Cmd::PageDown, "Move", "Page down";
    [(^'b')] => Cmd::PageUp, "Move", "Page up";
    [PageUp] => Cmd::PageUp, "Move", "Page up";
    ['w'] => Cmd::NextColumn, "Move", "Sort by next column";
    ['b'] => Cmd::PrevColumn, "Move", "Sort by previous column";
    ['H'] => Cmd::ScreenTop, "Move", "Top visible item";
    ['M'] => Cmd::ScreenMiddle, "Move", "Middle visible item";
    ['L'] => Cmd::ScreenBottom, "Move", "Bottom visible item";
    [Enter] => Cmd::Open, "Inspect", "Inspect selected entity / follow relationship";
    ['z', 'o'] => Cmd::Expand, "Inspect", "Expand branch";
    ['z', 'c'] => Cmd::Collapse, "Inspect", "Collapse branch";
    ['z', 'R'] => Cmd::ExpandAll, "Inspect", "Expand all";
    ['z', 'M'] => Cmd::CollapseAll, "Inspect", "Collapse all";
    ['t'] => Cmd::Tree, "Inspect", "Toggle process tree";
    [(^'o')] => Cmd::Back, "Inspect", "Back to previous location";
    [(^'i')] => Cmd::Forward, "Inspect", "Forward to next location";
    ['/'] => Cmd::SearchForward, "Search", "Search forward";
    ['?'] => Cmd::SearchBackward, "Search", "Search backward";
    ['n'] => Cmd::NextMatch, "Search", "Next match";
    ['N'] => Cmd::PrevMatch, "Search", "Previous match";
    ['F'] => Cmd::Filter, "Search", "Filter rows (e.g. cpu>10 user:root)";
    ['v'] => Cmd::VisualMode, "Select", "Visual (range) selection";
    [' '] => Cmd::ToggleSelect, "Select", "Toggle selection";
    ['y'] => Cmd::Yank, "Select", "Copy entity information";
    ['s', 'c'] => Cmd::Sort(SortKey::Cpu), "Sort", "Sort by CPU";
    ['s', 'm'] => Cmd::Sort(SortKey::Mem), "Sort", "Sort by resident memory";
    ['s', 'p'] => Cmd::Sort(SortKey::Pss), "Sort", "Sort by proportional memory";
    ['s', 'v'] => Cmd::Sort(SortKey::Virt), "Sort", "Sort by virtual memory";
    ['s', 'i'] => Cmd::Sort(SortKey::Io), "Sort", "Sort by storage I/O";
    ['s', 'n'] => Cmd::Sort(SortKey::Net), "Sort", "Sort by network activity";
    ['s', 'g'] => Cmd::Sort(SortKey::Gpu), "Sort", "Sort by GPU activity";
    ['s', 't'] => Cmd::Sort(SortKey::Threads), "Sort", "Sort by thread count";
    ['s', 'a'] => Cmd::Sort(SortKey::Age), "Sort", "Sort by age";
    ['s', 'd'] => Cmd::Sort(SortKey::Pid), "Sort", "Sort by PID";
    ['s', 'r'] => Cmd::SortReverse, "Sort", "Reverse sort order";
    ['s', '?'] => Cmd::SortMenu, "Sort", "List sort keys";
    ['x'] => Cmd::Terminate, "Act", "Terminate (SIGTERM, confirm)";
    ['X'] => Cmd::ForceKill, "Act", "Force kill (SIGKILL, immediate)";
    ['g', 's'] => Cmd::SignalMenu, "Act", "Send any signal";
    ['g', 'p'] => Cmd::Suspend, "Act", "Suspend (SIGSTOP)";
    ['g', 'r'] => Cmd::Resume, "Act", "Resume (SIGCONT)";
    ['m'] => Cmd::ContextMenu, "Act", "Action menu for selection";
    ['!'] => Cmd::Emergency, "Act", "Emergency mode (minimal collection)";
    ['p'] => Cmd::Pause, "Act", "Pause / resume updates";
    ['+'] => Cmd::Faster, "Act", "Refresh faster";
    ['-'] => Cmd::Slower, "Act", "Refresh slower";
    [':'] => Cmd::CommandLine, "General", "Command mode (:help, :sort, :filter, :signal, :q)";
    [Esc] => Cmd::Cancel, "General", "Cancel, deselect (view stays put), close, back";
    ['q'] => Cmd::Quit, "General", "Close view, quit from root";
    [(^'c')] => Cmd::ForceQuit, "General", "Quit";
    [(F 10)] => Cmd::ForceQuit, "General", "Quit";
    [(F 1)] => Cmd::Help, "General", "Help";
    [(F 9)] => Cmd::Terminate, "General", "Terminate (SIGTERM)";
    [(^'z')] => Cmd::SuspendRtop, "General", "Suspend rtop to the shell";
}

/// Result of feeding one key to the resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Cmd(Cmd),
    /// Prefix of a multi-key binding; wait for more keys.
    Pending,
    Unbound,
}

/// Resolves `pending` + `key` against [`BINDINGS`].
pub fn resolve(pending: &[Key], key: Key) -> Resolution {
    let mut seq: Vec<Key> = pending.to_vec();
    seq.push(normalise(key));
    let mut prefix = false;
    for b in BINDINGS {
        if b.keys == seq.as_slice() {
            return Resolution::Cmd(b.cmd);
        }
        if b.keys.len() > seq.len() && b.keys.starts_with(&seq) {
            prefix = true;
        }
    }
    if prefix {
        Resolution::Pending
    } else {
        Resolution::Unbound
    }
}

/// Terminals report Shift+letter as the uppercase letter with or without a
/// shift flag; alt is not part of any binding.
fn normalise(k: Key) -> Key {
    Key { alt: false, ..k }
}

/// Human-readable key sequence for help (`gg`, `Ctrl+d`, `F10`).
pub fn describe(keys: &[Key]) -> String {
    keys.iter()
        .map(|k| {
            let base = match k.code {
                KeyCode::Char(' ') => "Space".to_owned(),
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Enter => "Enter".into(),
                KeyCode::Esc => "Esc".into(),
                KeyCode::Backspace => "Backspace".into(),
                KeyCode::Tab => "Tab".into(),
                KeyCode::BackTab => "Shift+Tab".into(),
                KeyCode::Up => "↑".into(),
                KeyCode::Down => "↓".into(),
                KeyCode::Left => "←".into(),
                KeyCode::Right => "→".into(),
                KeyCode::Home => "Home".into(),
                KeyCode::End => "End".into(),
                KeyCode::PageUp => "PgUp".into(),
                KeyCode::PageDown => "PgDn".into(),
                KeyCode::Delete => "Del".into(),
                KeyCode::F(n) => format!("F{n}"),
            };
            if k.ctrl { format!("Ctrl+{base}") } else { base }
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_resolve() {
        assert_eq!(resolve(&[], Key::ch('g')), Resolution::Pending);
        assert_eq!(
            resolve(&[Key::ch('g')], Key::ch('g')),
            Resolution::Cmd(Cmd::Top)
        );
        assert_eq!(
            resolve(&[Key::ch('s')], Key::ch('m')),
            Resolution::Cmd(Cmd::Sort(SortKey::Mem))
        );
        assert_eq!(
            resolve(&[], Key::ch('5')),
            Resolution::Cmd(Cmd::View(View::Network))
        );
        assert_eq!(
            resolve(&[], Key::ctrl('d')),
            Resolution::Cmd(Cmd::HalfPageDown)
        );
        assert_eq!(resolve(&[Key::ch('s')], Key::ch('Q')), Resolution::Unbound);
    }

    #[test]
    fn no_binding_shadows_another() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert!(a.keys != b.keys, "duplicate binding {}", describe(a.keys));
                let (short, long) = if a.keys.len() < b.keys.len() {
                    (a, b)
                } else {
                    (b, a)
                };
                assert!(
                    short.keys.len() == long.keys.len() || !long.keys.starts_with(short.keys),
                    "{} shadows {}",
                    describe(short.keys),
                    describe(long.keys)
                );
            }
        }
    }
}
