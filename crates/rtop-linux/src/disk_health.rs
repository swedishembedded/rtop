// Copyright (c) 2026 Martin Schröder <info@swedishembedded.com>
//
// SPDX-License-Identifier: Apache-2.0
//
// Swedish Embedded AB implements solutions for storage reliability
// monitoring on Linux for its clients. If your team needs expertise in
// NVMe and ATA device health then you can procure our services by sending
// an email to info@swedishembedded.com.

//! Disk health: NVMe SMART / Health Information log and ATA SMART data.
//!
//! Both are read with strictly read-only commands: NVMe *Get Log Page*
//! (log 0x02) through `NVME_IOCTL_ADMIN_CMD`, and ATA *SMART READ DATA*
//! (and *RETURN STATUS*) through SCSI ATA PASS-THROUGH (`SG_IO`). Opening the
//! device nodes normally needs root; without access the provider reports
//! permission denied. Callers should poll rarely: these commands can wake a
//! drive from a low-power state.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::fs::KernelError;

/// NVMe SMART / Health Information (log identifier 0x02), NVMe base spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NvmeSmart {
    /// Bit 0 spare below threshold, 1 temperature, 2 reliability degraded,
    /// 3 read-only, 4 volatile backup failed, 5 persistent memory read-only.
    pub critical_warning: u8,
    pub temperature_kelvin: u16,
    pub available_spare_percent: u8,
    pub available_spare_threshold_percent: u8,
    /// Vendor estimate of life used; may exceed 100.
    pub percentage_used: u8,
    /// Units of 1000 x 512 bytes.
    pub data_units_read: u128,
    pub data_units_written: u128,
    pub power_cycles: u128,
    pub power_on_hours: u128,
    pub unsafe_shutdowns: u128,
    pub media_errors: u128,
    pub error_log_entries: u128,
}

fn u128_le(b: &[u8], off: usize) -> u128 {
    u128::from_le_bytes(b[off..off + 16].try_into().expect("16 bytes"))
}

/// Parses the 512-byte SMART / Health log page.
pub fn parse_nvme_smart(b: &[u8]) -> Option<NvmeSmart> {
    if b.len() < 512 {
        return None;
    }
    Some(NvmeSmart {
        critical_warning: b[0],
        temperature_kelvin: u16::from_le_bytes([b[1], b[2]]),
        available_spare_percent: b[3],
        available_spare_threshold_percent: b[4],
        percentage_used: b[5],
        data_units_read: u128_le(b, 32),
        data_units_written: u128_le(b, 48),
        power_cycles: u128_le(b, 112),
        power_on_hours: u128_le(b, 128),
        unsafe_shutdowns: u128_le(b, 144),
        media_errors: u128_le(b, 160),
        error_log_entries: u128_le(b, 176),
    })
}

/// One ATA SMART attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtaAttribute {
    pub id: u8,
    /// Normalised current value (higher is better).
    pub value: u8,
    pub worst: u8,
    /// Vendor raw value (48 bits).
    pub raw: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AtaSmart {
    pub attributes: Vec<AtaAttribute>,
    /// SMART RETURN STATUS: `Some(false)` when the drive reports a threshold
    /// exceeded (failing).
    pub healthy: Option<bool>,
}

impl AtaSmart {
    pub fn raw(&self, id: u8) -> Option<u64> {
        self.attributes.iter().find(|a| a.id == id).map(|a| a.raw)
    }
}

/// Parses the 512-byte SMART READ DATA structure (30 attribute slots of 12
/// bytes starting at offset 2).
pub fn parse_ata_smart(b: &[u8]) -> Option<Vec<AtaAttribute>> {
    if b.len() < 512 {
        return None;
    }
    Some(
        (0..30)
            .map(|i| &b[2 + i * 12..2 + (i + 1) * 12])
            .filter(|e| e[0] != 0)
            .map(|e| AtaAttribute {
                id: e[0],
                value: e[3],
                worst: e[4],
                raw: e[5..11]
                    .iter()
                    .rev()
                    .fold(0u64, |acc, &x| (acc << 8) | u64::from(x)),
            })
            .collect(),
    )
}

/// Source of disk health data.
pub trait DiskHealthSource: Send + Sync + std::fmt::Debug {
    /// `controller` is e.g. `nvme0`.
    fn nvme(&self, controller: &str) -> Result<NvmeSmart, KernelError>;
    /// `device` is e.g. `sda`.
    fn ata(&self, device: &str) -> Result<AtaSmart, KernelError>;
}

fn open_dev(name: &str) -> Result<OwnedFd, KernelError> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(KernelError::Invalid("device name"));
    }
    let path = std::ffi::CString::new(format!("/dev/{name}"))
        .map_err(|_| KernelError::Invalid("device name"))?;
    // SAFETY: valid path; read-only, non-blocking open.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(KernelError::from_io(&std::io::Error::last_os_error()));
    }
    // SAFETY: fresh fd owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `struct nvme_passthru_cmd` (linux/nvme_ioctl.h).
#[repr(C)]
#[derive(Default)]
struct NvmeAdminCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

/// `_IOWR('N', 0x41, struct nvme_admin_cmd)`.
const NVME_IOCTL_ADMIN_CMD: u64 = 0xC048_4E41;
const SG_IO: u64 = 0x2285;

/// `sg_io_hdr_t` (scsi/sg.h).
#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

/// Real devices.
#[derive(Debug, Default)]
pub struct DeviceHealth;

impl DeviceHealth {
    /// ATA PASS-THROUGH(16) of a SMART subcommand; returns the sense data.
    fn ata_smart_cmd(
        fd: &OwnedFd,
        feature: u8,
        data: Option<&mut [u8; 512]>,
    ) -> Result<[u8; 32], KernelError> {
        let (protocol, flags, dir, len, ptr) = match data {
            // PIO data-in, T_DIR=from device, BYTE_BLOCK, T_LENGTH=sector count.
            Some(buf) => (
                4u8 << 1,
                0x0e,
                -3,
                512u32,
                buf.as_mut_ptr().cast::<libc::c_void>(),
            ),
            // Non-data, CK_COND to get the result registers back in sense.
            None => (3u8 << 1, 0x20, -1, 0, std::ptr::null_mut()),
        };
        let mut cdb: [u8; 16] = [
            0x85, protocol, flags, 0, feature, 0, 1, 0, 0, 0, 0x4f, 0, 0xc2, 0, 0xb0, 0,
        ];
        let mut sense = [0u8; 32];
        let mut hdr = SgIoHdr {
            interface_id: i32::from(b'S'),
            dxfer_direction: dir,
            cmd_len: 16,
            mx_sb_len: sense.len() as u8,
            iovec_count: 0,
            dxfer_len: len,
            dxferp: ptr,
            cmdp: cdb.as_mut_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: 5000,
            flags: 0,
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        // SAFETY: hdr points at live buffers of the declared sizes.
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), SG_IO as _, &mut hdr) };
        if r < 0 {
            return Err(KernelError::from_io(&std::io::Error::last_os_error()));
        }
        if hdr.host_status != 0 {
            return Err(KernelError::Unsupported);
        }
        Ok(sense)
    }
}

impl DiskHealthSource for DeviceHealth {
    fn nvme(&self, controller: &str) -> Result<NvmeSmart, KernelError> {
        let fd = open_dev(controller)?;
        let mut buf = [0u8; 512];
        let mut cmd = NvmeAdminCmd {
            opcode: 0x02, // Get Log Page
            nsid: 0xffff_ffff,
            addr: buf.as_mut_ptr() as u64,
            data_len: buf.len() as u32,
            // NUMDL = dwords - 1, log identifier 0x02.
            cdw10: ((buf.len() as u32 / 4 - 1) << 16) | 0x02,
            timeout_ms: 5000,
            ..Default::default()
        };
        // SAFETY: cmd describes a live 512-byte buffer.
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), NVME_IOCTL_ADMIN_CMD as _, &mut cmd) };
        if r < 0 {
            return Err(KernelError::from_io(&std::io::Error::last_os_error()));
        }
        if r > 0 {
            return Err(KernelError::Unsupported); // NVMe status code
        }
        parse_nvme_smart(&buf).ok_or(KernelError::Invalid("nvme log"))
    }

    fn ata(&self, device: &str) -> Result<AtaSmart, KernelError> {
        let fd = open_dev(device)?;
        let mut buf = [0u8; 512];
        Self::ata_smart_cmd(&fd, 0xd0, Some(&mut buf))?; // SMART READ DATA
        let attributes = parse_ata_smart(&buf).ok_or(KernelError::Invalid("ata smart"))?;
        // SMART RETURN STATUS: LBA mid/high 0x2c/0xf4 means threshold exceeded.
        let healthy = Self::ata_smart_cmd(&fd, 0xda, None).ok().and_then(|sense| {
            // Descriptor-format sense, ATA Status Return descriptor (0x09).
            (sense[8] == 0x09).then(|| !(sense[17] == 0xf4 && sense[15] == 0x2c))
        });
        Ok(AtaSmart {
            attributes,
            healthy,
        })
    }
}

/// No device access (fixtures, containers without device nodes).
#[derive(Debug)]
pub struct NoDiskHealth;

impl DiskHealthSource for NoDiskHealth {
    fn nvme(&self, _: &str) -> Result<NvmeSmart, KernelError> {
        Err(KernelError::Unsupported)
    }
    fn ata(&self, _: &str) -> Result<AtaSmart, KernelError> {
        Err(KernelError::Unsupported)
    }
}

/// Builds a log page with the given values (tests and the demo machine).
pub fn encode_nvme_smart(s: &NvmeSmart) -> [u8; 512] {
    let mut b = [0u8; 512];
    b[0] = s.critical_warning;
    b[1..3].copy_from_slice(&s.temperature_kelvin.to_le_bytes());
    b[3] = s.available_spare_percent;
    b[4] = s.available_spare_threshold_percent;
    b[5] = s.percentage_used;
    for (off, v) in [
        (32, s.data_units_read),
        (48, s.data_units_written),
        (112, s.power_cycles),
        (128, s.power_on_hours),
        (144, s.unsafe_shutdowns),
        (160, s.media_errors),
        (176, s.error_log_entries),
    ] {
        b[off..off + 16].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// Builds a SMART READ DATA structure (tests and the demo machine).
pub fn encode_ata_smart(attrs: &[AtaAttribute]) -> [u8; 512] {
    let mut b = [0u8; 512];
    for (i, a) in attrs.iter().take(30).enumerate() {
        let e = &mut b[2 + i * 12..2 + (i + 1) * 12];
        e[0] = a.id;
        e[3] = a.value;
        e[4] = a.worst;
        for k in 0..6 {
            e[5 + k] = (a.raw >> (8 * k)) as u8;
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn nvme_log_fields_are_at_spec_offsets() {
        let s = NvmeSmart {
            critical_warning: 0b100,
            temperature_kelvin: 318,
            available_spare_percent: 97,
            available_spare_threshold_percent: 10,
            percentage_used: 4,
            data_units_read: 7,
            data_units_written: 1 << 70,
            power_cycles: 812,
            power_on_hours: 4321,
            unsafe_shutdowns: 33,
            media_errors: 2,
            error_log_entries: 9,
        };
        let b = encode_nvme_smart(&s);
        assert_eq!(b[5], 4);
        assert_eq!(u16::from_le_bytes([b[1], b[2]]), 318);
        assert_eq!(parse_nvme_smart(&b), Some(s));
        assert_eq!(parse_nvme_smart(&b[..100]), None);
    }

    #[test]
    fn ata_attributes_round_trip_and_raw_is_little_endian_48_bit() {
        let attrs = [
            AtaAttribute {
                id: 5,
                value: 100,
                worst: 100,
                raw: 3,
            },
            AtaAttribute {
                id: 194,
                value: 64,
                worst: 40,
                raw: 0x0000_2e00_0024,
            },
            AtaAttribute {
                id: 9,
                value: 90,
                worst: 90,
                raw: 0xffff_ffff_ffff,
            },
        ];
        let parsed = parse_ata_smart(&encode_ata_smart(&attrs)).unwrap();
        assert_eq!(parsed, attrs);
    }

    #[test]
    fn struct_layouts_match_the_kernel_abi() {
        assert_eq!(std::mem::size_of::<NvmeAdminCmd>(), 72);
        assert_eq!(std::mem::size_of::<SgIoHdr>(), 88);
        assert!(
            open_dev("../etc/passwd").is_err(),
            "device names are validated"
        );
    }

    proptest! {
        #[test]
        fn parsers_never_panic(b in proptest::collection::vec(any::<u8>(), 0..600)) {
            let _ = parse_nvme_smart(&b);
            let _ = parse_ata_smart(&b);
        }
    }
}
