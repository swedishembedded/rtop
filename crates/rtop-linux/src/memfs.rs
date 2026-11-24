// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0

//! In-memory kernel filesystem with fault injection.
//!
//! Used to simulate machines of any size (thousands of CPUs or processes)
//! and to inject the failures real kernels produce: permission errors,
//! files vanishing mid-scan, oversized content, slow and indefinitely
//! blocking reads. Contents can be changed between samples to model
//! counters advancing, devices disappearing and PID reuse.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use crate::fs::{FsUsage, KernelError, KernelFs};

/// A barrier that blocks reads until opened; models a hung driver or NFS.
#[derive(Debug, Default)]
pub struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Gate::default())
    }

    pub fn open(&self) {
        *self.open.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.cv.notify_all();
    }

    /// Blocks until [`Gate::open`] is called.
    pub fn wait(&self) {
        let mut g = self.open.lock().unwrap_or_else(|p| p.into_inner());
        while !*g {
            g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
        }
    }
}

#[derive(Debug, Clone)]
enum Entry {
    File(Vec<u8>),
    Link(String),
    Dir,
    Fail(KernelError),
    Slow(Duration, Vec<u8>),
    Blocked(Arc<Gate>, Vec<u8>),
}

#[derive(Debug, Default)]
pub struct MemoryKernelFs {
    entries: RwLock<BTreeMap<String, Entry>>,
    usage: RwLock<BTreeMap<String, Result<FsUsage, KernelError>>>,
    live: bool,
}

fn norm(path: &str) -> String {
    let t = path.trim_end_matches('/');
    if t.is_empty() {
        "/".into()
    } else {
        t.to_owned()
    }
}

impl MemoryKernelFs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pretends to be the live system (enables action code paths in tests
    /// that use a mock process controller).
    pub fn with_live(mut self, live: bool) -> Self {
        self.live = live;
        self
    }

    fn put(&self, path: &str, e: Entry) {
        self.entries
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(norm(path), e);
    }

    pub fn file(&self, path: &str, content: impl AsRef<[u8]>) -> &Self {
        self.put(path, Entry::File(content.as_ref().to_vec()));
        self
    }

    pub fn link(&self, path: &str, target: &str) -> &Self {
        self.put(path, Entry::Link(target.to_owned()));
        self
    }

    pub fn dir(&self, path: &str) -> &Self {
        self.put(path, Entry::Dir);
        self
    }

    pub fn fail(&self, path: &str, err: KernelError) -> &Self {
        self.put(path, Entry::Fail(err));
        self
    }

    pub fn slow(&self, path: &str, delay: Duration, content: impl AsRef<[u8]>) -> &Self {
        self.put(path, Entry::Slow(delay, content.as_ref().to_vec()));
        self
    }

    /// Reads of `path` block until `gate` is opened.
    pub fn blocked(&self, path: &str, gate: Arc<Gate>, content: impl AsRef<[u8]>) -> &Self {
        self.put(path, Entry::Blocked(gate, content.as_ref().to_vec()));
        self
    }

    /// Removes `path` and everything below it.
    pub fn remove(&self, path: &str) -> &Self {
        let p = norm(path);
        let prefix = format!("{p}/");
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| *k != p && !k.starts_with(&prefix));
        self
    }

    pub fn set_usage(&self, mount: &str, usage: Result<FsUsage, KernelError>) -> &Self {
        self.usage
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(norm(mount), usage);
        self
    }

    /// Loads a captured fixture tree (regular files and symlinks).
    pub fn load_dir(&self, root: &Path) -> std::io::Result<()> {
        fn walk(fs: &MemoryKernelFs, root: &Path, dir: &Path) -> std::io::Result<()> {
            for ent in std::fs::read_dir(dir)? {
                let ent = ent?;
                let path = ent.path();
                let rel = format!(
                    "/{}",
                    path.strip_prefix(root)
                        .expect("below root")
                        .to_string_lossy()
                );
                let ft = ent.file_type()?;
                if ft.is_symlink() {
                    fs.link(&rel, &std::fs::read_link(&path)?.to_string_lossy());
                } else if ft.is_dir() {
                    fs.dir(&rel);
                    walk(fs, root, &path)?;
                } else {
                    fs.file(&rel, std::fs::read(&path)?);
                }
            }
            Ok(())
        }
        walk(self, root, root)
    }

    fn get(&self, path: &str) -> Option<Entry> {
        self.entries
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&norm(path))
            .cloned()
    }
}

impl KernelFs for MemoryKernelFs {
    fn read(&self, path: &str, buf: &mut Vec<u8>, limit: usize) -> Result<(), KernelError> {
        buf.clear();
        let data = match self.get(path) {
            Some(Entry::File(d)) => d,
            Some(Entry::Slow(delay, d)) => {
                std::thread::sleep(delay);
                d
            }
            Some(Entry::Blocked(gate, d)) => {
                gate.wait();
                d
            }
            Some(Entry::Fail(e)) => return Err(e),
            Some(Entry::Dir) => return Err(KernelError::Os(libc::EISDIR)),
            Some(Entry::Link(_)) => return Err(KernelError::Unsupported),
            None => return Err(KernelError::NotFound),
        };
        if data.len() > limit {
            return Err(KernelError::TooLarge { limit });
        }
        buf.extend_from_slice(&data);
        Ok(())
    }

    fn read_dir(&self, path: &str) -> Result<Vec<String>, KernelError> {
        let p = norm(path);
        if let Some(Entry::Fail(e)) = self.get(&p) {
            return Err(e);
        }
        let prefix = if p == "/" {
            "/".to_owned()
        } else {
            format!("{p}/")
        };
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let mut names = BTreeSet::new();
        for k in entries
            .range(prefix.clone()..)
            .map(|(k, _)| k)
            .take_while(|k| k.starts_with(&prefix))
        {
            if let Some(first) = k[prefix.len()..]
                .split('/')
                .next()
                .filter(|s| !s.is_empty())
            {
                names.insert(first.to_owned());
            }
        }
        if names.is_empty() && !matches!(entries.get(&p), Some(Entry::Dir)) {
            return Err(KernelError::NotFound);
        }
        Ok(names.into_iter().collect())
    }

    fn read_link(&self, path: &str) -> Result<String, KernelError> {
        match self.get(path) {
            Some(Entry::Link(t)) => Ok(t),
            Some(Entry::Fail(e)) => Err(e),
            Some(_) => Err(KernelError::Os(libc::EINVAL)),
            None => Err(KernelError::NotFound),
        }
    }

    fn statvfs(&self, path: &str) -> Result<FsUsage, KernelError> {
        if let Some(Entry::Blocked(gate, _)) = self.get(&format!("statvfs:{}", norm(path))) {
            gate.wait();
        }
        self.usage
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&norm(path))
            .cloned()
            .unwrap_or(Err(KernelError::Unsupported))
    }

    fn is_live(&self) -> bool {
        self.live
    }
}

impl MemoryKernelFs {
    /// Makes `statvfs(mount)` block until `gate` opens (hung NFS server).
    pub fn block_statvfs(&self, mount: &str, gate: Arc<Gate>) -> &Self {
        self.put(
            &format!("statvfs:{}", norm(mount)),
            Entry::Blocked(gate, Vec::new()),
        );
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_are_implied_and_removal_is_recursive() {
        let fs = MemoryKernelFs::new();
        fs.file("/proc/1/stat", "x")
            .file("/proc/2/stat", "y")
            .file("/proc/stat", "z");
        let mut d = fs.read_dir("/proc").unwrap();
        d.sort();
        assert_eq!(d, ["1", "2", "stat"]);
        fs.remove("/proc/1");
        assert_eq!(fs.read_dir("/proc").unwrap().len(), 2);
        assert_eq!(fs.read_dir("/nope"), Err(KernelError::NotFound));
    }

    #[test]
    fn gate_blocks_until_opened() {
        let fs = Arc::new(MemoryKernelFs::new());
        let gate = Gate::new();
        fs.blocked("/sys/hang", gate.clone(), "ok");
        let f2 = fs.clone();
        let h = std::thread::spawn(move || {
            let mut b = Vec::new();
            f2.read("/sys/hang", &mut b, 10).map(|_| b)
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!h.is_finished());
        gate.open();
        assert_eq!(h.join().unwrap().unwrap(), b"ok");
    }
}
