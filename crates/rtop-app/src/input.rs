// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Terminal-independent input events.

use crate::view::{Panel, RowId, SortKey, SplitId, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

impl Key {
    pub const fn plain(code: KeyCode) -> Key {
        Key {
            code,
            ctrl: false,
            alt: false,
        }
    }

    pub const fn ch(c: char) -> Key {
        Key::plain(KeyCode::Char(c))
    }

    pub const fn ctrl(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
        }
    }
}

/// A clickable region identified by the renderer for the last frame.
///
/// Rows carry the stable [`RowId`] that was *displayed* there, so a click
/// targets what the user saw even if the table has since been re-sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Tab(View),
    Row { panel: Panel, row: RowId },
    Header { panel: Panel, sort: SortKey },
    Panel(Panel),
    MenuItem(usize),
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseAction {
    Click(Hit),
    /// Ctrl-click: toggle one row in the selection set.
    ToggleClick(Hit),
    /// Shift-click: select the range from the cursor.
    RangeClick(Hit),
    DoubleClick(Hit),
    RightClick {
        hit: Hit,
        x: u16,
        y: u16,
    },
    Scroll {
        panel: Option<Panel>,
        lines: i32,
    },
    /// Click outside any modal: closes it.
    Dismiss,
    /// A pane border was dragged: the boundary now sits at `permille`
    /// thousandths of the layout it divides.
    Resize {
        split: SplitId,
        permille: u16,
    },
    /// Double click on a pane border: back to the layout's own proportions.
    ResetSplit(SplitId),
}
