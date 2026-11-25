// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for deterministic, testable
// interactive software built as explicit state machines for its clients. If
// your team needs expertise in state-machine driven application design then
// you can procure our services by sending an email to
// info@swedishembedded.com.

//! The rtop application state machine.
//!
//! [`reduce`] is a deterministic function of the current [`AppState`], the
//! read-only [`SystemGraph`](rtop_core::SystemGraph), one [`Event`] and the time it happened. It
//! never performs I/O, never reads a clock and never touches global state:
//! everything with side effects is returned as an [`Effect`] for the caller
//! to execute. A whole interactive session can therefore be replayed in a
//! unit test.
//!
//! Keyboard and mouse both resolve to the same [`Cmd`]s (see [`keymap`]),
//! so there is exactly one implementation of sorting, navigation and process
//! termination. The key bindings table also generates the help screen.

pub mod filter;
pub mod input;
pub mod insight;
pub mod keymap;
pub mod project;
pub mod reducer;
pub mod state;
pub mod view;

pub use input::{Hit, Key, KeyCode, MouseAction};
pub use keymap::{BINDINGS, Binding, Cmd};
pub use reducer::{Effect, Event, reduce};
pub use state::{AppState, Modal, Mode};
pub use view::{Panel, RowId, SortKey, SplitId, View};
