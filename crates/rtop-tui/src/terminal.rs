// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The real terminal.
//!
//! * Input is read on its own thread and delivered through a bounded
//!   channel; mouse motion is dropped under backpressure, keys never are.
//! * Output goes through a writer thread with room for two frames. When the
//!   terminal stops reading (a stuck SSH session, a paused tmux pane) the
//!   event loop skips rendering instead of blocking, and repaints fully once
//!   the writer catches up. Input handling and process actions therefore
//!   never wait for the terminal.
//! * The terminal is restored on exit, on panic and around Ctrl+Z.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, Event as CtEvent, KeyCode as CtKey, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::{cursor, execute, terminal as ct};
use rtop_app::input::{Key, KeyCode};
use rtop_app::{Hit, MouseAction};

use crate::hits::HitMap;

/// Raw input delivered by the input thread.
#[derive(Debug, Clone)]
pub enum Input {
    Key(Key),
    Mouse(MouseEvent),
    Resize(u16, u16),
}

pub fn convert_key(k: event::KeyEvent) -> Option<Key> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let code = match k.code {
        CtKey::Char(c) => KeyCode::Char(if ctrl { c.to_ascii_lowercase() } else { c }),
        CtKey::Enter => KeyCode::Enter,
        CtKey::Esc => KeyCode::Esc,
        CtKey::Backspace => KeyCode::Backspace,
        CtKey::Tab => KeyCode::Tab,
        CtKey::BackTab => KeyCode::BackTab,
        CtKey::Up => KeyCode::Up,
        CtKey::Down => KeyCode::Down,
        CtKey::Left => KeyCode::Left,
        CtKey::Right => KeyCode::Right,
        CtKey::Home => KeyCode::Home,
        CtKey::End => KeyCode::End,
        CtKey::PageUp => KeyCode::PageUp,
        CtKey::PageDown => KeyCode::PageDown,
        CtKey::Delete => KeyCode::Delete,
        CtKey::F(n) => KeyCode::F(n),
        _ => return None,
    };
    Some(Key { code, ctrl, alt })
}

/// Starts the input thread.
pub fn spawn_input(stop: Arc<AtomicBool>) -> Receiver<Input> {
    let (tx, rx) = bounded::<Input>(512);
    let _ = std::thread::Builder::new()
        .name("rtop-input".into())
        .spawn(move || {
            rtop_linux::sys::set_thread_nice(-10);
            while !stop.load(Ordering::Relaxed) {
                // Poll so the thread can notice shutdown and suspension.
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break,
                }
                let input = match event::read() {
                    Ok(CtEvent::Key(k)) => convert_key(k).map(Input::Key),
                    Ok(CtEvent::Mouse(m)) => Some(Input::Mouse(m)),
                    Ok(CtEvent::Resize(w, h)) => Some(Input::Resize(w, h)),
                    Ok(_) => None,
                    Err(_) => break,
                };
                let Some(input) = input else { continue };
                let res = match &input {
                    Input::Mouse(m)
                        if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) =>
                    {
                        match tx.try_send(input) {
                            Err(TrySendError::Disconnected(_)) => Err(()),
                            _ => Ok(()),
                        }
                    }
                    _ => tx.send(input).map_err(|_| ()),
                };
                if res.is_err() {
                    break;
                }
            }
        });
    rx
}

/// Turns raw mouse events into semantic actions using the hit map of the
/// frame the user was looking at.
#[derive(Debug, Default)]
pub struct MouseTracker {
    last_click: Option<(Instant, Hit)>,
}

impl MouseTracker {
    pub fn translate(&mut self, m: MouseEvent, hits: &HitMap, now: Instant) -> Option<MouseAction> {
        let (x, y) = (m.column, m.row);
        if hits.modal.is_some() && !hits.in_modal(x, y) && matches!(m.kind, MouseEventKind::Down(_))
        {
            return Some(MouseAction::Dismiss);
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = hits.at(x, y)?.clone();
                if m.modifiers.contains(KeyModifiers::CONTROL) {
                    return Some(MouseAction::ToggleClick(hit));
                }
                if m.modifiers.contains(KeyModifiers::SHIFT) {
                    return Some(MouseAction::RangeClick(hit));
                }
                let double = self.last_click.as_ref().is_some_and(|(t, h)| {
                    *h == hit && now.duration_since(*t) < Duration::from_millis(400)
                });
                if double {
                    self.last_click = None;
                    Some(MouseAction::DoubleClick(hit))
                } else {
                    self.last_click = Some((now, hit.clone()));
                    Some(MouseAction::Click(hit))
                }
            }
            MouseEventKind::Down(MouseButton::Right) => Some(MouseAction::RightClick {
                hit: hits.at(x, y)?.clone(),
                x,
                y,
            }),
            MouseEventKind::ScrollDown => Some(MouseAction::Scroll {
                panel: hits.panel_at(x, y),
                lines: 3,
            }),
            MouseEventKind::ScrollUp => Some(MouseAction::Scroll {
                panel: hits.panel_at(x, y),
                lines: -3,
            }),
            _ => None,
        }
    }
}

/// `Write` adapter that hands complete frames to the writer thread.
struct FrameWriter {
    buf: Vec<u8>,
    tx: Sender<Vec<u8>>,
    dropped: Arc<AtomicBool>,
}

impl Write for FrameWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let frame = std::mem::take(&mut self.buf);
        if let Err(TrySendError::Full(_)) = self.tx.try_send(frame) {
            // The terminal is not keeping up: this frame is lost, so the
            // next one must repaint everything.
            self.dropped.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Crossterm backend that never queries the terminal.
///
/// Ratatui asks for the cursor position when clearing; that round-trip
/// fails on terminals that do not answer and blocks on a stalled one. rtop
/// owns the whole alternate screen with the cursor hidden, so the position
/// is irrelevant and is reported as the origin.
struct NoQueryBackend(CrosstermBackend<FrameWriter>);

impl ratatui::backend::Backend for NoQueryBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.0.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<ratatui::layout::Position> {
        Ok(ratatui::layout::Position::ORIGIN)
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(&mut self, p: P) -> io::Result<()> {
        self.0.set_cursor_position(p)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }
    fn clear_region(&mut self, t: ratatui::backend::ClearType) -> io::Result<()> {
        self.0.clear_region(t)
    }
    fn size(&self) -> io::Result<ratatui::layout::Size> {
        self.0.size()
    }
    fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
        self.0.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.0)
    }
}

pub struct Term {
    terminal: Terminal<NoQueryBackend>,
    tx: Sender<Vec<u8>>,
    dropped: Arc<AtomicBool>,
    mouse: bool,
}

impl std::fmt::Debug for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Term")
    }
}

fn enter(mouse: bool) -> io::Result<()> {
    ct::enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, ct::EnterAlternateScreen, cursor::Hide)?;
    if mouse {
        execute!(out, event::EnableMouseCapture)?;
    }
    Ok(())
}

/// Puts the terminal back the way we found it. Safe to call repeatedly.
pub fn restore() {
    let mut out = io::stdout();
    let _ = execute!(
        out,
        event::DisableMouseCapture,
        ct::LeaveAlternateScreen,
        cursor::Show
    );
    let _ = ct::disable_raw_mode();
}

impl Term {
    pub fn start(mouse: bool) -> io::Result<Term> {
        enter(mouse)?;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        let (tx, rx) = bounded::<Vec<u8>>(2);
        std::thread::Builder::new()
            .name("rtop-output".into())
            .spawn(move || {
                rtop_linux::sys::set_thread_nice(-5);
                let mut out = io::stdout();
                for frame in rx {
                    if out.write_all(&frame).and_then(|_| out.flush()).is_err() {
                        break;
                    }
                }
            })?;
        let dropped = Arc::new(AtomicBool::new(false));
        let writer = FrameWriter {
            buf: Vec::with_capacity(1 << 16),
            tx: tx.clone(),
            dropped: dropped.clone(),
        };
        let mut terminal = Terminal::new(NoQueryBackend(CrosstermBackend::new(writer)))?;
        terminal.clear()?;
        Ok(Term {
            terminal,
            tx,
            dropped,
            mouse,
        })
    }

    /// Whether a frame can be queued without waiting for the terminal.
    pub fn ready(&self) -> bool {
        self.tx.len() < self.tx.capacity().unwrap_or(2)
    }

    /// Draws if the terminal can take a frame; returns whether it drew.
    pub fn draw(&mut self, f: impl FnOnce(&mut ratatui::Frame)) -> io::Result<bool> {
        if !self.ready() {
            return Ok(false);
        }
        if self.dropped.swap(false, Ordering::Relaxed) {
            self.terminal.clear()?;
        }
        self.terminal.draw(f)?;
        Ok(true)
    }

    pub fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .map(|s| (s.width, s.height))
            .unwrap_or((80, 24))
    }

    pub fn resized(&mut self) -> io::Result<()> {
        self.terminal.autoresize()
    }

    /// OSC 52 clipboard copy (works over SSH in most terminals).
    pub fn copy(&mut self, text: &str) {
        let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
        let _ = self.tx.try_send(seq.into_bytes());
    }

    /// Ctrl+Z: restore the terminal, stop, and take it back on resume.
    pub fn suspend(&mut self) -> io::Result<()> {
        restore();
        // SAFETY: raising a signal on ourselves.
        unsafe {
            libc_raise_tstp();
        }
        enter(self.mouse)?;
        self.terminal.clear()
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        restore();
    }
}

unsafe fn libc_raise_tstp() {
    // SIGTSTP's default action stops the process until SIGCONT.
    unsafe extern "C" {
        fn raise(sig: i32) -> i32;
    }
    const SIGTSTP: i32 = 20;
    unsafe {
        raise(SIGTSTP);
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn double_click_requires_same_target_quickly() {
        use ratatui::layout::Rect;
        let mut hits = HitMap::default();
        let row = Hit::Row {
            panel: rtop_app::Panel::Processes,
            row: rtop_app::RowId::Collector("x"),
        };
        hits.push(Rect::new(0, 0, 10, 1), row.clone());
        let ev = |k| MouseEvent {
            kind: k,
            column: 1,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let mut t = MouseTracker::default();
        let now = Instant::now();
        assert_eq!(
            t.translate(ev(MouseEventKind::Down(MouseButton::Left)), &hits, now),
            Some(MouseAction::Click(row.clone()))
        );
        assert_eq!(
            t.translate(
                ev(MouseEventKind::Down(MouseButton::Left)),
                &hits,
                now + Duration::from_millis(100)
            ),
            Some(MouseAction::DoubleClick(row.clone()))
        );
        assert_eq!(
            t.translate(
                ev(MouseEventKind::Down(MouseButton::Left)),
                &hits,
                now + Duration::from_secs(2)
            ),
            Some(MouseAction::Click(row))
        );
    }
}
