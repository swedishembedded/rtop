// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Frame-specific registry of clickable regions.

use ratatui::layout::Rect;
use rtop_app::Hit;
use rtop_app::view::Panel;

#[derive(Debug, Default, Clone)]
pub struct HitMap {
    hits: Vec<(Rect, Hit)>,
    panels: Vec<(Rect, Panel)>,
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
