// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! End-to-end UI journeys: the compiled binary in a real pseudo-terminal,
//! driven with keystrokes, its screen interpreted by a terminal emulator.
//! Signals are only ever sent to child processes these tests start.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Tui {
    child: Child,
    master: std::fs::File,
    screen: Arc<Mutex<vt100::Parser>>,
    raw: Arc<Mutex<Vec<u8>>>,
}

fn nz(v: u16) -> std::num::NonZero<u16> {
    std::num::NonZero::new(v.max(1)).expect("non-zero")
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Tui {
    fn launch(args: &[&str], cols: u16, rows: u16) -> Tui {
        let (mut master, mut slave) = (0, 0);
        let ws = winsize(cols, rows);
        // SAFETY: openpty writes two fds; null name/termios are allowed.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        };
        assert_eq!(r, 0, "openpty");
        // SAFETY: fresh descriptors owned here.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        let fd = slave.as_raw_fd();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rtop"));
        cmd.args(args)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                libc::ioctl(fd, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn rtop");
        let master = std::fs::File::from(master);
        let screen = Arc::new(Mutex::new(vt100::Parser::new(nz(rows), nz(cols), 0)));
        let raw = Arc::new(Mutex::new(Vec::new()));
        let (mut reader, s2, r2) = (master.try_clone().unwrap(), screen.clone(), raw.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                s2.lock().unwrap().process(&buf[..n]);
                r2.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Tui {
            child,
            master,
            screen,
            raw,
        }
    }

    fn text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }

    fn send(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
        std::thread::sleep(Duration::from_millis(120));
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.text().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn expect(&self, needle: &str) {
        assert!(
            self.wait_for(needle, Duration::from_secs(10)),
            "'{needle}' never appeared; screen:\n{}",
            self.text()
        );
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        // SAFETY: valid fd and winsize.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        self.screen
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(nz(rows), nz(cols));
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(s) = self.child.try_wait().unwrap() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A test-owned victim process with a unique, searchable command line.
fn victim(tag: &str) -> (Child, String) {
    let name = format!("rtop-e2e-{tag}-{}", std::process::id());
    let child = Command::new("sleep")
        .arg0(&name)
        .arg("600")
        .spawn()
        .expect("spawn victim");
    (child, name)
}

fn proc_state(pid: u32) -> Option<char> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    s[s.rfind(')')? + 2..].chars().next()
}

fn wait_gone(child: &mut Child, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn find_victim(t: &mut Tui, name: &str) {
    t.expect("Memory");
    t.send("1");
    t.expect("Processes");
    t.send("F");
    t.send(name);
    t.send("\r");
    t.expect(name);
    // Back in normal mode with the victim selected before acting.
    t.expect("NORMAL");
    t.expect("Processes 1 ");
}

#[test]
fn ux_launch_navigate_every_view_and_quit() {
    let mut t = Tui::launch(&["--demo"], 140, 40);
    t.expect("Memory");
    for (key, title) in [
        ("1", "Processes"),
        ("2", "Logical CPUs"),
        ("3", "Memory"),
        ("4", "Block devices"),
        ("5", "Communication"),
        ("6", "Containers"),
        ("7", "GPUs"),
        ("8", "Telemetry sources"),
        ("9", "Sensors"),
        ("0", "Insights"),
    ] {
        t.send(key);
        t.expect(title);
    }
    t.send("q");
    assert!(
        t.wait_exit(Duration::from_secs(5))
            .expect("rtop exits on q")
            .success()
    );
    // Terminal restored: alternate screen left, cursor shown.
    let raw = t.raw.lock().unwrap().clone();
    let s = String::from_utf8_lossy(&raw);
    assert!(s.contains("\x1b[?1049l") && s.contains("\x1b[?25h"));
}

#[test]
fn ux_search_filter_inspect_and_back() {
    let mut t = Tui::launch(&["--demo"], 160, 48);
    t.expect("Memory");
    t.send("1Fuser:dbuser\r");
    t.expect("filter: user:dbuser");
    assert!(!t.text().contains("rustc"), "filtered out");
    t.send("\r");
    t.expect("Relationships");
    t.expect("Deep inspection");
    t.send("\x1b");
    t.expect("filter: user:dbuser");
    t.send(":q\r");
    assert!(t.wait_exit(Duration::from_secs(5)).is_some());
}

#[test]
fn ux_terminate_live_child_with_confirmation() {
    let (mut v, name) = victim("term");
    let mut t = Tui::launch(&[], 160, 48);
    find_victim(&mut t, &name);
    t.send("x");
    t.expect("Terminate process?");
    t.send("y");
    assert!(
        wait_gone(&mut v, Duration::from_secs(10)),
        "victim terminated"
    );
    t.expect("SIGTERM");
}

#[test]
fn ux_cancelled_termination_sends_nothing() {
    let (mut v, name) = victim("cancel");
    let mut t = Tui::launch(&[], 160, 48);
    find_victim(&mut t, &name);
    t.send("x");
    t.expect("Terminate process?");
    t.send("\x1b");
    t.expect("cancelled");
    std::thread::sleep(Duration::from_millis(500));
    assert!(v.try_wait().unwrap().is_none(), "victim must still run");
    v.kill().unwrap();
    v.wait().unwrap();
}

#[test]
fn ux_force_kill_is_immediate_and_suspend_resume_work() {
    let (mut v, name) = victim("kill");
    let pid = v.id();
    let mut t = Tui::launch(&[], 160, 48);
    find_victim(&mut t, &name);
    t.send("gp");
    let end = Instant::now() + Duration::from_secs(5);
    while proc_state(pid) != Some('T') && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(proc_state(pid), Some('T'), "suspended");
    t.send("gr");
    let end = Instant::now() + Duration::from_secs(5);
    while proc_state(pid) == Some('T') && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_ne!(proc_state(pid), Some('T'), "resumed");
    t.send("X");
    assert!(
        wait_gone(&mut v, Duration::from_secs(5)),
        "SIGKILL without dialog"
    );
}

#[test]
fn ux_resize_and_sigterm_restore_terminal() {
    let mut t = Tui::launch(&["--demo"], 120, 36);
    t.expect("Memory");
    for (c, r) in [(50, 16), (200, 60), (20, 6), (80, 24)] {
        t.resize(c, r);
        // SAFETY: SIGWINCH to our child.
        unsafe { libc::kill(t.child.id() as i32, libc::SIGWINCH) };
        std::thread::sleep(Duration::from_millis(300));
    }
    t.expect("rtop");
    // SAFETY: signalling the rtop child we started.
    unsafe { libc::kill(t.child.id() as i32, libc::SIGTERM) };
    assert!(
        t.wait_exit(Duration::from_secs(5)).is_some(),
        "exits on SIGTERM"
    );
    let s = String::from_utf8_lossy(&t.raw.lock().unwrap()).into_owned();
    assert!(
        s.contains("\x1b[?1049l"),
        "alternate screen left after SIGTERM"
    );
}

#[test]
fn ux_emergency_mode_and_help() {
    let mut t = Tui::launch(&["--demo"], 140, 40);
    t.expect("Memory");
    t.send("!");
    t.expect("EMERGENCY");
    t.send("!");
    t.expect("LIVE");
    t.send("\x1bOP"); // F1
    t.expect("rtop help");
    t.send("\x1b");
    t.send("q");
    assert!(t.wait_exit(Duration::from_secs(5)).is_some());
}

/// Busy-looping children that saturate every CPU while alive.
struct CpuHogs(Vec<Child>);

impl CpuHogs {
    fn start() -> CpuHogs {
        let n = std::thread::available_parallelism().map_or(4, |n| n.get());
        CpuHogs(
            (0..n)
                .map(|_| {
                    Command::new("sh")
                        .args(["-c", "while :; do :; done"])
                        .spawn()
                        .expect("spawn hog")
                })
                .collect(),
        )
    }
}

impl Drop for CpuHogs {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn ux_emergency_kill_latency_under_cpu_saturation() {
    let (mut v, name) = victim("latency");
    let mut t = Tui::launch(&[], 160, 48);
    find_victim(&mut t, &name);
    let _hogs = CpuHogs::start();
    std::thread::sleep(Duration::from_millis(500));
    // Keypress to observed exit, measured from outside rtop.
    let start = Instant::now();
    t.master.write_all(b"X").unwrap();
    assert!(wait_gone(&mut v, Duration::from_secs(10)), "victim killed");
    let latency = start.elapsed();
    eprintln!("emergency kill latency under CPU saturation: {latency:?}");
    // Specification target: 100 ms p99 with an optimised build. Debug
    // builds and shared CI machines get a looser bound.
    let budget = if cfg!(debug_assertions) {
        Duration::from_millis(1000)
    } else {
        Duration::from_millis(150)
    };
    assert!(latency < budget, "{latency:?} exceeds {budget:?}");
}
