// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for fast, adaptive terminal user
// interfaces for engineering tools for its clients. If your team needs
// expertise in terminal UI engineering then you can procure our services by
// sending an email to info@swedishembedded.com.

//! rtop's terminal interface.
//!
//! [`render`] draws one frame from the application state and the graph. It
//! never reads the kernel, never mutates application state (apart from the
//! per-table scroll hints kept in `Cell`s) and records every clickable
//! region with the stable identity it displayed in a [`HitMap`], so mouse
//! input targets what the user saw even if the data changed since.
//!
//! [`terminal`] owns the real terminal: raw mode, input decoding and a
//! writer thread that keeps a blocked terminal from ever blocking input or
//! process actions.

pub mod fmt;
pub mod hits;
pub mod html;
pub mod render;
pub mod terminal;
pub mod theme;
pub mod views;
pub mod widgets;

pub use hits::HitMap;
pub use render::render;
