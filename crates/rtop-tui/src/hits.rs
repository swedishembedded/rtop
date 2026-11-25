// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Frame-specific registry of clickable regions.

use ratatui::layout::Rect;
use rtop_app::view::Panel;
use rtop_app::{Hit, SplitId};

/// Direction in which the panes of a split are stacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Panes one above the other; the border is a horizontal line that
    /// moves up and down.
    Vertical,
    /// Panes side by side; the border is a vertical line that moves left
    /// and right.
    Horizontal,
}

/// A draggable border between two adjacent panes, as drawn in the last
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splitter {
    pub id: SplitId,
    pub axis: Axis,
    /// The two cells thick border: the first pane's last line and the
    /// second pane's first line.
    pub line: Rect,
    /// Coordinate along `axis` where the second pane starts.
    pub boundary: u16,
    /// Start and length along `axis` of the layout the boundary divides.
    pub start: u16,
    pub len: u16,
}

impl Splitter {
    /// Coordinate of a point along this splitter's axis.
    pub fn along(&self, x: u16, y: u16) -> u16 {
        match self.axis {
            Axis::Vertical => y,
            Axis::Horizontal => x,
        }
    }

    /// The boundary position, in thousandths of the layout, that puts the
    /// second pane's start at `boundary`.
    pub fn permille(&self, boundary: i32) -> u16 {
        let len = i32::from(self.len.max(1));
        let off = (boundary - i32::from(self.start)).clamp(0, len);
        ((off * 1000 + len / 2) / len) as u16
    }
}

/// Where a boundary stored as `permille` of a layout of `len` cells
/// starting at `start` falls; the inverse of [`Splitter::permille`].
pub fn boundary_at(start: u16, len: u16, permille: u16) -> u16 {
    let off = (u32::from(len) * u32::from(permille.min(1000)) + 500) / 1000;
    start + off as u16
}

#[derive(Debug, Default, Clone)]
pub struct HitMap {
    hits: Vec<(Rect, Hit)>,
    panels: Vec<(Rect, Panel)>,
    splitters: Vec<Splitter>,
    /// Area of the open modal, if any: clicks outside it dismiss it.
    pub modal: Option<Rect>,
}

fn inside(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

impl HitMap {
    pub fn clear(&mut self) {
        self.hits.clear();
        self.panels.clear();
        self.splitters.clear();
        self.modal = None;
    }

    pub fn push(&mut self, r: Rect, h: Hit) {
        if r.width > 0 && r.height > 0 {
            self.hits.push((r, h));
        }
    }

    pub fn panel(&mut self, r: Rect, p: Panel) {
        self.panels.push((r, p));
    }

    /// Topmost hit at a position (later registrations draw on top).
    pub fn at(&self, x: u16, y: u16) -> Option<&Hit> {
        self.hits
            .iter()
            .rev()
            .find(|(r, _)| inside(*r, x, y))
            .map(|(_, h)| h)
    }

    pub fn panel_at(&self, x: u16, y: u16) -> Option<Panel> {
        self.panels
            .iter()
            .rev()
            .find(|(r, _)| inside(*r, x, y))
            .map(|(_, p)| *p)
    }

    /// Area of a panel drawn in the last frame.
    pub fn panel_rect(&self, p: Panel) -> Option<Rect> {
        self.panels.iter().find(|(_, q)| *q == p).map(|(r, _)| *r)
    }

    pub fn panel_rects(&self) -> impl Iterator<Item = Rect> + '_ {
        self.panels.iter().map(|(r, _)| *r)
    }

    pub fn splitter(&mut self, s: Splitter) {
        if s.line.width > 0 && s.line.height > 0 {
            self.splitters.push(s);
        }
    }

    /// The border under a position; a modal covers the borders beneath it.
    pub fn splitter_at(&self, x: u16, y: u16) -> Option<&Splitter> {
        if self.modal.is_some() {
            return None;
        }
        self.splitters.iter().find(|s| inside(s.line, x, y))
    }

    pub fn splitters(&self) -> impl Iterator<Item = &Splitter> {
        self.splitters.iter()
    }

    pub fn in_modal(&self, x: u16, y: u16) -> bool {
        self.modal.is_some_and(|m| inside(m, x, y))
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Rect, Hit)> {
        self.hits.iter()
    }
}
