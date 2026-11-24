// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for safe, race-free process
// management on Linux for its clients. If your team needs expertise in
// Linux systems programming then you can procure our services by sending an
// email to info@swedishembedded.com.

//! Signalling processes without ever hitting the wrong one.
//!
//! A target is a [`ProcessIdentity`] (PID + start time), never a bare PID or
//! a table row. Delivery opens a pidfd, then re-reads the start time of the
//! process the pidfd refers to: the pidfd pins that exact process, so if the
//! start time matches, the signal sent through it cannot reach a recycled
//! PID. Any mismatch aborts. Kernels without pidfd (< 5.3) get no
//! destructive actions at all rather than a racy fallback.
//!
//! Delivery is not success: after SIGTERM/SIGKILL the controller observes
//! whether the process actually exited, and reports a process stuck in
//! uninterruptible sleep as still present.

use std::time::Duration;

use rtop_core::ProcessIdentity;
use rtop_linux::KernelError;
use rtop_linux::fs::{KernelFs, KernelFsExt};
use rtop_linux::parse::pid;
use rtop_linux::sys::PidFd;

/// What happened to one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Signal delivered; for terminating signals the process exited.
    Exited,
    /// Signal delivered (non-terminating signal, or exit not awaited).
    Delivered,
    /// Delivered, but the process is still present after the grace period.
    StillRunning {
        state: String,
    },
    /// The identity no longer matches a live process; nothing was sent.
    Gone,
    /// Refused by the kernel.
    PermissionDenied,
    /// Not attempted (actions disabled, no pidfd support, ...).
    Refused(String),
    Failed(String),
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Outcome::Exited => "exited".into(),
            Outcome::Delivered => "signal delivered".into(),
            Outcome::StillRunning { state } if state == "disk_sleep" => "still present: in uninterruptible sleep (D); it exits when its kernel wait completes".into(),
            Outcome::StillRunning { state } => format!("still running ({state}); escalate with X (SIGKILL)"),
            Outcome::Gone => "not sent: the process already exited or its PID was reused".into(),
            Outcome::PermissionDenied => "permission denied".into(),
            Outcome::Refused(why) => format!("not sent: {why}"),
            Outcome::Failed(e) => format!("failed: {e}"),
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Exited | Outcome::Delivered)
    }
}

/// Executes signals. Implementations must validate identity themselves.
pub trait ProcessController: Send + std::fmt::Debug {
    fn signal(&self, target: ProcessIdentity, signal: i32) -> Outcome;
}

/// How long to wait for exit after a terminating signal.
pub fn grace_period(signal: i32) -> Option<Duration> {
    match signal {
        libc::SIGKILL => Some(Duration::from_millis(500)),
        libc::SIGTERM | libc::SIGINT | libc::SIGQUIT | libc::SIGHUP => {
            Some(Duration::from_millis(1000))
        }
        _ => None,
    }
}

/// pidfd-based controller for the live system.
#[derive(Debug)]
pub struct PidfdController<F: KernelFs> {
    fs: F,
}

impl<F: KernelFs> PidfdController<F> {
    pub fn new(fs: F) -> Self {
        PidfdController { fs }
    }

    fn start_time(&self, p: u32) -> Option<(u64, u8)> {
        let mut buf = Vec::new();
        self.fs
            .read(&format!("/proc/{p}/stat"), &mut buf, 64 * 1024)
            .ok()?;
        pid::stat(&buf).map(|s| (s.starttime, s.state))
    }
}

impl<F: KernelFs> ProcessController for PidfdController<F> {
    fn signal(&self, target: ProcessIdentity, signal: i32) -> Outcome {
        let fd = match PidFd::open(target.pid) {
            Ok(fd) => fd,
            Err(KernelError::NotFound) => return Outcome::Gone,
            Err(KernelError::Unsupported) => {
                return Outcome::Refused("kernel lacks pidfd_open (needs Linux 5.3+)".into());
            }
            Err(KernelError::PermissionDenied) => return Outcome::PermissionDenied,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        // The pidfd now pins one specific process; verify it is ours.
        match self.start_time(target.pid) {
            Some((start, _)) if start == target.start_time_ticks => {}
            _ => return Outcome::Gone,
        }
        match fd.send_signal(signal) {
            Ok(()) => {}
            Err(KernelError::NotFound) => return Outcome::Gone,
            Err(KernelError::PermissionDenied) => return Outcome::PermissionDenied,
            Err(e) => return Outcome::Failed(e.to_string()),
        }
        let Some(grace) = grace_period(signal) else {
            return Outcome::Delivered;
        };
        if fd.wait_exit(grace) {
            return Outcome::Exited;
        }
        // Zombies have exited; anything else is still there.
        match self.start_time(target.pid) {
            Some((start, state)) if start == target.start_time_ticks && state != b'Z' => {
                Outcome::StillRunning {
                    state: pid::state_name(state).into(),
                }
            }
            _ => Outcome::Exited,
        }
    }
}

/// Refuses everything (replays, fixtures, restricted mode).
#[derive(Debug)]
pub struct DisabledController(pub String);

impl ProcessController for DisabledController {
    fn signal(&self, _: ProcessIdentity, _: i32) -> Outcome {
        Outcome::Refused(self.0.clone())
    }
}

/// Whether this kernel supports pidfd-based actions (probed on ourselves).
pub fn pidfd_supported() -> bool {
    PidFd::open(std::process::id()).is_ok()
}

/// Reads the identity of a live PID (for CLI use by PID).
pub fn identity_of(fs: &dyn KernelFs, p: u32) -> Option<ProcessIdentity> {
    let s = fs.read_string(&format!("/proc/{p}/stat"), 64 * 1024).ok()?;
    pid::stat(s.as_bytes()).map(|st| ProcessIdentity {
        pid: p,
        start_time_ticks: st.starttime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtop_linux::RealKernelFs;

    fn spawn() -> (std::process::Child, ProcessIdentity) {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let id = identity_of(&RealKernelFs::live(), child.id()).unwrap();
        (child, id)
    }

    #[test]
    fn terminate_observes_exit() {
        let (mut child, id) = spawn();
        let c = PidfdController::new(RealKernelFs::live());
        let reaper = std::thread::spawn(move || child.wait());
        assert_eq!(c.signal(id, libc::SIGTERM), Outcome::Exited);
        reaper.join().unwrap().unwrap();
    }

    #[test]
    fn stale_identity_is_never_signalled() {
        let (mut child, id) = spawn();
        let c = PidfdController::new(RealKernelFs::live());
        // Same PID, different start time: simulated PID reuse.
        let wrong = ProcessIdentity {
            pid: id.pid,
            start_time_ticks: id.start_time_ticks + 1,
        };
        assert_eq!(c.signal(wrong, libc::SIGKILL), Outcome::Gone);
        assert!(
            child.try_wait().unwrap().is_none(),
            "victim must be untouched"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(c.signal(id, libc::SIGKILL), Outcome::Gone);
    }

    #[test]
    fn stop_and_continue_are_delivered() {
        let (mut child, id) = spawn();
        let c = PidfdController::new(RealKernelFs::live());
        assert_eq!(c.signal(id, libc::SIGSTOP), Outcome::Delivered);
        std::thread::sleep(Duration::from_millis(50));
        let state = RealKernelFs::live()
            .read_string(&format!("/proc/{}/stat", id.pid), 4096)
            .unwrap();
        assert_eq!(pid::stat(state.as_bytes()).unwrap().state, b'T');
        assert_eq!(c.signal(id, libc::SIGCONT), Outcome::Delivered);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
