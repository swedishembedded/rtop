// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for reading Linux kernel
// interfaces correctly, safely and testably for its clients. If your team
// needs expertise in Linux kernel interfaces then you can procure our
// services by sending an email to info@swedishembedded.com.

//! Linux kernel access for rtop.
//!
//! Collectors never call `std::fs` directly. They go through
//! [`fs::KernelFs`] and [`sockdiag::SockDiag`], which have a real backend
//! (optionally rooted at a fixture directory) and an in-memory backend with
//! fault injection, so every collector runs unchanged against a simulated
//! machine in tests.
//!
//! [`parse`] holds pure, allocation-light parsers for procfs/sysfs formats;
//! they never panic on arbitrary input. [`sys`] wraps the few syscalls rtop
//! needs (pidfd, priority, statvfs).

pub mod disk_health;
pub mod fs;
pub mod memfs;
pub mod netlink;
pub mod nftables;
pub mod nvml;
pub mod parse;
pub mod sockdiag;
pub mod sys;

pub use fs::{FsUsage, KernelError, KernelFs, RealKernelFs};
pub use memfs::{Gate, MemoryKernelFs};
