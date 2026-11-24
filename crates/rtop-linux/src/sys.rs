// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Thin, safe wrappers around the syscalls rtop needs.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crate::fs::{FsUsage, KernelError};

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn err() -> KernelError {
    KernelError::from_io(&std::io::Error::last_os_error())
}

pub fn statvfs(path: &Path) -> Result<FsUsage, KernelError> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| KernelError::Invalid("path"))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out-struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(err());
    }
    let frsize = s.f_frsize as u64;
    Ok(FsUsage {
        total_bytes: (s.f_blocks as u64).saturating_mul(frsize),
        free_bytes: (s.f_bfree as u64).saturating_mul(frsize),
        available_bytes: (s.f_bavail as u64).saturating_mul(frsize),
        total_inodes: s.f_files as u64,
        free_inodes: s.f_ffree as u64,
    })
}

/// Clock ticks per second (`USER_HZ`) used by `/proc/PID/stat` times.
pub fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no memory-safety preconditions.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

pub fn page_size() -> u64 {
    // SAFETY: as above.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 { v as u64 } else { 4096 }
}

pub fn gettid() -> i32 {
    // SAFETY: gettid cannot fail.
    unsafe { libc::gettid() }
}

pub fn getuid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// Sets the nice value of the calling *thread* (Linux nice is per-thread)
/// and returns the value actually in effect afterwards. Lowering below the
/// current value needs `CAP_SYS_NICE` or `RLIMIT_NICE`; on refusal the
/// thread keeps its current priority and that value is returned.
pub fn set_thread_nice(nice: i32) -> i32 {
    let tid = gettid() as libc::id_t;
    // SAFETY: setpriority/getpriority on our own thread id.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, tid, nice);
    }
    thread_nice()
}

pub fn thread_nice() -> i32 {
    let tid = gettid() as libc::id_t;
    // getpriority can legitimately return -1, so errno must be cleared.
    // SAFETY: errno location is thread-local and valid.
    unsafe {
        *libc::__errno_location() = 0;
        let v = libc::getpriority(libc::PRIO_PROCESS, tid);
        if v == -1 && errno() != 0 { 0 } else { v }
    }
}

/// A stable handle to a process that cannot be redirected by PID reuse.
#[derive(Debug)]
pub struct PidFd {
    fd: OwnedFd,
    pub pid: u32,
}

impl PidFd {
    /// `pidfd_open(2)`. Fails with `Unsupported` on kernels < 5.3.
    pub fn open(pid: u32) -> Result<PidFd, KernelError> {
        let pid_t = libc::pid_t::try_from(pid).map_err(|_| KernelError::Invalid("pid"))?;
        // SAFETY: raw syscall with integer arguments.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_t, 0) };
        if fd < 0 {
            return Err(match errno() {
                libc::ENOSYS => KernelError::Unsupported,
                _ => err(),
            });
        }
        // SAFETY: the kernel returned a fresh fd we now own.
        Ok(PidFd {
            fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            pid,
        })
    }

    /// `pidfd_send_signal(2)`: delivered to exactly the process this handle
    /// was opened for, or fails with `NotFound` if it has exited.
    pub fn send_signal(&self, signal: i32) -> Result<(), KernelError> {
        // SAFETY: valid owned fd, null siginfo, zero flags.
        let r = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if r < 0 { Err(err()) } else { Ok(()) }
    }

    /// Waits up to `timeout` for the process to exit (pidfd becomes
    /// readable). `true` if it exited.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one valid pollfd.
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        r > 0 && pfd.revents & libc::POLLIN != 0
    }
}

/// A Linux signal known to this architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalInfo {
    pub number: i32,
    pub name: &'static str,
    pub description: &'static str,
}

/// Every standard signal plus the real-time range, numbered by this
/// architecture's libc constants rather than hard-coded values.
pub fn signals() -> Vec<SignalInfo> {
    let mut v = vec![
        SignalInfo {
            number: libc::SIGTERM,
            name: "SIGTERM",
            description: "Request termination",
        },
        SignalInfo {
            number: libc::SIGKILL,
            name: "SIGKILL",
            description: "Unconditionally terminate",
        },
        SignalInfo {
            number: libc::SIGINT,
            name: "SIGINT",
            description: "Interrupt (Ctrl+C)",
        },
        SignalInfo {
            number: libc::SIGHUP,
            name: "SIGHUP",
            description: "Hangup or reload configuration",
        },
        SignalInfo {
            number: libc::SIGQUIT,
            name: "SIGQUIT",
            description: "Quit with core dump",
        },
        SignalInfo {
            number: libc::SIGSTOP,
            name: "SIGSTOP",
            description: "Suspend execution",
        },
        SignalInfo {
            number: libc::SIGCONT,
            name: "SIGCONT",
            description: "Resume execution",
        },
        SignalInfo {
            number: libc::SIGTSTP,
            name: "SIGTSTP",
            description: "Terminal stop (Ctrl+Z)",
        },
        SignalInfo {
            number: libc::SIGUSR1,
            name: "SIGUSR1",
            description: "Application-defined 1",
        },
        SignalInfo {
            number: libc::SIGUSR2,
            name: "SIGUSR2",
            description: "Application-defined 2",
        },
        SignalInfo {
            number: libc::SIGABRT,
            name: "SIGABRT",
            description: "Abort with core dump",
        },
        SignalInfo {
            number: libc::SIGALRM,
            name: "SIGALRM",
            description: "Timer expired",
        },
        SignalInfo {
            number: libc::SIGPIPE,
            name: "SIGPIPE",
            description: "Broken pipe",
        },
        SignalInfo {
            number: libc::SIGCHLD,
            name: "SIGCHLD",
            description: "Child state changed",
        },
        SignalInfo {
            number: libc::SIGWINCH,
            name: "SIGWINCH",
            description: "Terminal window changed",
        },
        SignalInfo {
            number: libc::SIGTTIN,
            name: "SIGTTIN",
            description: "Background terminal read",
        },
        SignalInfo {
            number: libc::SIGTTOU,
            name: "SIGTTOU",
            description: "Background terminal write",
        },
        SignalInfo {
            number: libc::SIGURG,
            name: "SIGURG",
            description: "Urgent socket data",
        },
        SignalInfo {
            number: libc::SIGXCPU,
            name: "SIGXCPU",
            description: "CPU time limit exceeded",
        },
        SignalInfo {
            number: libc::SIGXFSZ,
            name: "SIGXFSZ",
            description: "File size limit exceeded",
        },
        SignalInfo {
            number: libc::SIGVTALRM,
            name: "SIGVTALRM",
            description: "Virtual timer expired",
        },
        SignalInfo {
            number: libc::SIGPROF,
            name: "SIGPROF",
            description: "Profiling timer expired",
        },
        SignalInfo {
            number: libc::SIGIO,
            name: "SIGIO",
            description: "I/O possible",
        },
        SignalInfo {
            number: libc::SIGPWR,
            name: "SIGPWR",
            description: "Power failure",
        },
        SignalInfo {
            number: libc::SIGSYS,
            name: "SIGSYS",
            description: "Bad system call",
        },
        SignalInfo {
            number: libc::SIGTRAP,
            name: "SIGTRAP",
            description: "Trace/breakpoint trap",
        },
        SignalInfo {
            number: libc::SIGBUS,
            name: "SIGBUS",
            description: "Bus error",
        },
        SignalInfo {
            number: libc::SIGFPE,
            name: "SIGFPE",
            description: "Arithmetic exception",
        },
        SignalInfo {
            number: libc::SIGILL,
            name: "SIGILL",
            description: "Illegal instruction",
        },
        SignalInfo {
            number: libc::SIGSEGV,
            name: "SIGSEGV",
            description: "Segmentation violation",
        },
    ];
    const RT_NAMES: [&str; 33] = [
        "SIGRTMIN",
        "SIGRTMIN+1",
        "SIGRTMIN+2",
        "SIGRTMIN+3",
        "SIGRTMIN+4",
        "SIGRTMIN+5",
        "SIGRTMIN+6",
        "SIGRTMIN+7",
        "SIGRTMIN+8",
        "SIGRTMIN+9",
        "SIGRTMIN+10",
        "SIGRTMIN+11",
        "SIGRTMIN+12",
        "SIGRTMIN+13",
        "SIGRTMIN+14",
        "SIGRTMIN+15",
        "SIGRTMIN+16",
        "SIGRTMIN+17",
        "SIGRTMIN+18",
        "SIGRTMIN+19",
        "SIGRTMIN+20",
        "SIGRTMIN+21",
        "SIGRTMIN+22",
        "SIGRTMIN+23",
        "SIGRTMIN+24",
        "SIGRTMIN+25",
        "SIGRTMIN+26",
        "SIGRTMIN+27",
        "SIGRTMIN+28",
        "SIGRTMIN+29",
        "SIGRTMIN+30",
        "SIGRTMIN+31",
        "SIGRTMIN+32",
    ];
    let (lo, hi) = (libc::SIGRTMIN(), libc::SIGRTMAX());
    for (i, n) in (lo..=hi).enumerate() {
        if let Some(name) = RT_NAMES.get(i) {
            v.push(SignalInfo {
                number: n,
                name,
                description: "Real-time, application-defined",
            });
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pidfd_signals_only_the_opened_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let fd = PidFd::open(child.id()).unwrap();
        fd.send_signal(libc::SIGKILL).unwrap();
        assert!(fd.wait_exit(Duration::from_secs(5)));
        child.wait().unwrap();
        // After reaping, the handle refers to a dead process: no redirection.
        assert_eq!(fd.send_signal(libc::SIGKILL), Err(KernelError::NotFound));
    }

    #[test]
    fn signal_table_is_architecture_numbered_and_unique() {
        let s = signals();
        assert!(
            s.iter()
                .any(|x| x.name == "SIGKILL" && x.number == libc::SIGKILL)
        );
        let mut nums: Vec<_> = s.iter().map(|x| x.number).collect();
        nums.sort();
        nums.dedup();
        assert_eq!(nums.len(), s.len());
    }

    #[test]
    fn thread_nice_cannot_be_raised_above_19() {
        let handle = std::thread::spawn(|| set_thread_nice(30));
        assert_eq!(handle.join().unwrap(), 19);
    }

    #[test]
    fn statvfs_root() {
        let u = statvfs(Path::new("/")).unwrap();
        assert!(u.total_bytes >= u.free_bytes);
    }
}
