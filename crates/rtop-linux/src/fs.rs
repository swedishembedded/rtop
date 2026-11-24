// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! The kernel filesystem abstraction and its real implementation.

use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Default upper bound for a single kernel file read. Large tables (e.g.
/// `/proc/net/tcp` on a busy server) pass an explicit larger limit.
pub const DEFAULT_READ_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelError {
    NotFound,
    PermissionDenied,
    /// The file is larger than the caller's limit; nothing was returned.
    TooLarge {
        limit: usize,
    },
    /// The interface exists but the operation is not supported here.
    Unsupported,
    /// Any other OS error (errno).
    Os(i32),
    /// Content was present but malformed.
    Invalid(&'static str),
}

impl KernelError {
    pub fn from_io(e: &io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => KernelError::NotFound,
            io::ErrorKind::PermissionDenied => KernelError::PermissionDenied,
            _ => match e.raw_os_error() {
                Some(libc::ESRCH) => KernelError::NotFound,
                Some(libc::EACCES) | Some(libc::EPERM) => KernelError::PermissionDenied,
                Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::EINVAL) => {
                    KernelError::Unsupported
                }
                Some(n) => KernelError::Os(n),
                None => KernelError::Os(0),
            },
        }
    }
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::NotFound => f.write_str("not found"),
            KernelError::PermissionDenied => f.write_str("permission denied"),
            KernelError::TooLarge { limit } => write!(f, "larger than {limit} bytes"),
            KernelError::Unsupported => f.write_str("unsupported"),
            KernelError::Os(n) => write!(f, "{}", io::Error::from_raw_os_error(*n)),
            KernelError::Invalid(what) => write!(f, "invalid data: {what}"),
        }
    }
}

impl std::error::Error for KernelError {}

/// Filesystem capacity as reported by `statvfs()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsUsage {
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Available to unprivileged users (excludes reserved blocks).
    pub available_bytes: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
}

/// Read-only access to procfs, sysfs and other kernel-provided files.
///
/// Paths are absolute kernel paths such as `/proc/stat`. Implementations
/// must bound memory: reads beyond `limit` fail with
/// [`KernelError::TooLarge`] rather than allocate.
pub trait KernelFs: Send + Sync + fmt::Debug {
    fn read(&self, path: &str, buf: &mut Vec<u8>, limit: usize) -> Result<(), KernelError>;
    /// Entry names (not paths) of a directory, in unspecified order.
    fn read_dir(&self, path: &str) -> Result<Vec<String>, KernelError>;
    fn read_link(&self, path: &str) -> Result<String, KernelError>;
    /// Capacity of the filesystem mounted at `path`. May block on remote
    /// filesystems; only isolated collectors call it.
    fn statvfs(&self, path: &str) -> Result<FsUsage, KernelError>;

    /// Whether this backend observes the live machine (actions allowed).
    fn is_live(&self) -> bool {
        false
    }
}

/// Convenience helpers on top of [`KernelFs`].
pub trait KernelFsExt: KernelFs {
    fn read_string(&self, path: &str, limit: usize) -> Result<String, KernelError> {
        let mut buf = Vec::new();
        self.read(path, &mut buf, limit)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Small attribute file, trimmed (sysfs style).
    fn read_attr(&self, path: &str) -> Result<String, KernelError> {
        let mut s = self.read_string(path, 64 * 1024)?;
        let t = s.trim_end().len();
        s.truncate(t);
        Ok(s)
    }

    fn read_u64(&self, path: &str) -> Result<u64, KernelError> {
        self.read_attr(path)?
            .trim()
            .parse()
            .map_err(|_| KernelError::Invalid("integer"))
    }

    fn read_i64(&self, path: &str) -> Result<i64, KernelError> {
        self.read_attr(path)?
            .trim()
            .parse()
            .map_err(|_| KernelError::Invalid("integer"))
    }
}

impl<T: KernelFs + ?Sized> KernelFsExt for T {}

/// The real kernel, optionally rooted at a captured fixture tree.
#[derive(Debug, Clone)]
pub struct RealKernelFs {
    root: Option<PathBuf>,
}

impl RealKernelFs {
    /// The live system.
    pub fn live() -> Self {
        RealKernelFs { root: None }
    }

    /// A directory containing a captured `proc/`, `sys/`, ... tree. Rooted
    /// backends are never live: process actions are refused against them.
    pub fn rooted(root: impl Into<PathBuf>) -> Self {
        RealKernelFs {
            root: Some(root.into()),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        match &self.root {
            None => PathBuf::from(path),
            Some(r) => r.join(path.trim_start_matches('/')),
        }
    }
}

impl KernelFs for RealKernelFs {
    fn read(&self, path: &str, buf: &mut Vec<u8>, limit: usize) -> Result<(), KernelError> {
        buf.clear();
        let f = std::fs::File::open(self.resolve(path)).map_err(|e| KernelError::from_io(&e))?;
        // procfs reports size 0, so read incrementally up to limit + 1.
        let mut take = f.take(limit as u64 + 1);
        take.read_to_end(buf)
            .map_err(|e| KernelError::from_io(&e))?;
        if buf.len() > limit {
            buf.clear();
            return Err(KernelError::TooLarge { limit });
        }
        Ok(())
    }

    fn read_dir(&self, path: &str) -> Result<Vec<String>, KernelError> {
        let rd = std::fs::read_dir(self.resolve(path)).map_err(|e| KernelError::from_io(&e))?;
        let mut out = Vec::new();
        for ent in rd {
            match ent {
                Ok(e) => out.push(e.file_name().to_string_lossy().into_owned()),
                // An entry vanishing mid-iteration (exiting process) is normal.
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    fn read_link(&self, path: &str) -> Result<String, KernelError> {
        std::fs::read_link(self.resolve(path))
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| KernelError::from_io(&e))
    }

    fn statvfs(&self, path: &str) -> Result<FsUsage, KernelError> {
        if self.root.is_some() {
            // Capacity of a fixture directory says nothing about the
            // captured machine.
            return Err(KernelError::Unsupported);
        }
        crate::sys::statvfs(Path::new(path))
    }

    fn is_live(&self) -> bool {
        self.root.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_reads_fail_without_returning_partial_data() {
        let dir = std::env::temp_dir().join(format!("rtop-fs-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("proc")).unwrap();
        std::fs::write(dir.join("proc/big"), vec![b'x'; 100]).unwrap();
        let fs = RealKernelFs::rooted(&dir);
        let mut buf = Vec::new();
        assert_eq!(
            fs.read("/proc/big", &mut buf, 10),
            Err(KernelError::TooLarge { limit: 10 })
        );
        assert!(buf.is_empty());
        fs.read("/proc/big", &mut buf, 100).unwrap();
        assert_eq!(buf.len(), 100);
        assert_eq!(
            fs.read("/proc/missing", &mut buf, 10),
            Err(KernelError::NotFound)
        );
        assert!(!fs.is_live());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn live_backend_reads_real_procfs() {
        let fs = RealKernelFs::live();
        let s = fs
            .read_string("/proc/self/stat", DEFAULT_READ_LIMIT)
            .unwrap();
        assert!(s.starts_with(&std::process::id().to_string()));
    }
}
