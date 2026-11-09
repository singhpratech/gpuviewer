//! NVIDIA Xid errors from the kernel log.
//!
//! Xid is the NVIDIA driver's own error report ("GPU has fallen off the bus", "double bit
//! ECC error", a GSP timeout). NVML exposes no API for it: the only source a non-DCGM tool
//! has is the kernel log, where the driver prints lines like
//!
//! ```text
//! NVRM: Xid (PCI:0000:01:00): 79, pid=4521, name=python, GPU has fallen off the bus.
//! ```
//!
//! This module has two halves. [`parse_xid`] is pure and runs (and is tested) on every OS.
//! [`XidWatcher`] reads `/dev/kmsg` non-blockingly on Linux, starting at the END of the
//! buffer — an Xid from before this session is history the recorder did not witness, and
//! narrating it now would stamp it with the wrong time.
//!
//! Privilege: with `kernel.dmesg_restrict=1` (the Ubuntu/Debian default) reading the kernel
//! log needs CAP_SYSLOG. Then [`XidWatcher::open`] fails and the collector simply runs
//! without Xid events — `gpuviewer doctor` says so. Nothing is faked in its place.

/// One parsed Xid report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XidRecord {
    /// The PCI address as the driver printed it: `domain:bus:device`, no function
    /// (e.g. `0000:01:00`).
    pub pci: String,
    /// The Xid code (e.g. 79).
    pub code: u32,
    /// Everything after the code, verbatim (pid/name when the driver includes them, then
    /// its description). Never reinterpreted.
    pub detail: String,
    /// The full message as logged, for evidence.
    pub raw: String,
}

/// Parse one kernel-log message (either a raw `/dev/kmsg` record or a bare `dmesg`
/// message line) into an Xid report. `None` for anything that is not an Xid line.
pub fn parse_xid(line: &str) -> Option<XidRecord> {
    // /dev/kmsg records are `PRI,SEQ,TS,FLAGS;MESSAGE` followed by optional
    // continuation lines (` KEY=VALUE`). Keep only the first line's message part.
    let first = line.lines().next()?;
    let msg = match first.split_once(';') {
        Some((head, rest)) if head.split(',').count() >= 3 => rest,
        _ => first,
    };
    let at = msg.find("NVRM: Xid (PCI:")?;
    let after = &msg[at + "NVRM: Xid (PCI:".len()..];
    let (pci, rest) = after.split_once("):")?;
    let pci = pci.trim();
    let rest = rest.trim_start();
    let code_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let code: u32 = rest[..code_end].parse().ok()?;
    let detail = rest[code_end..]
        .trim_start_matches(',')
        .trim()
        .trim_end_matches('.')
        .to_string();
    if pci.is_empty() {
        return None;
    }
    Some(XidRecord {
        pci: pci.to_ascii_lowercase(),
        code,
        detail,
        raw: msg.trim_end().to_string(),
    })
}

/// Whether the Xid PCI address (`0000:01:00`, no function) names the device whose id
/// normalizes to `device_pci` (`0000:01:00.0`). Domain width is normalized the same way
/// [`crate::normalize_pci_id`] does it.
pub fn xid_matches_device(xid_pci: &str, device_id: &str) -> bool {
    let Some(dev) = crate::normalize_pci_id(device_id) else {
        return false;
    };
    let Some(xid) = crate::normalize_pci_id(&format!("{xid_pci}.0")) else {
        return false;
    };
    // Compare domain:bus:device; the function digit is not in the Xid line.
    dev.rsplit_once('.').map(|(p, _)| p) == xid.rsplit_once('.').map(|(p, _)| p)
}

/// Non-blocking reader over `/dev/kmsg`, positioned at the end of the buffer on open.
#[cfg(target_os = "linux")]
pub struct XidWatcher {
    file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl XidWatcher {
    /// Open the kernel log for reading. Fails with the OS error (typically EPERM under
    /// `kernel.dmesg_restrict=1` without CAP_SYSLOG) — a normal, explained outcome.
    pub fn open() -> std::io::Result<Self> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/kmsg")?;
        // Skip everything logged before this session (see module docs).
        file.seek(SeekFrom::End(0))?;
        Ok(Self { file })
    }

    /// Drain every record logged since the last call and return the Xid ones. Never
    /// blocks: an empty buffer ends the drain. A record overwritten before we read it
    /// (EPIPE — the ring wrapped) is skipped, not fatal.
    pub fn poll(&mut self) -> Vec<XidRecord> {
        use std::io::Read;
        let mut out = Vec::new();
        // One read() returns exactly one record; records are capped at ~8 KiB.
        let mut buf = vec![0u8; 8192];
        // Bound the drain so a log flood cannot stall a tick.
        for _ in 0..1024 {
            match self.file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(x) = parse_xid(&String::from_utf8_lossy(&buf[..n])) {
                        out.push(x);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
                Err(_) => break,
            }
        }
        out
    }
}

/// Probe whether Xid watching is possible on this host, for `gpuviewer doctor`:
/// `Ok(())` when the kernel log is readable, else the reason.
pub fn xid_watch_status() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        match XidWatcher::open() {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(
                "the kernel log is restricted (kernel.dmesg_restrict=1): run with CAP_SYSLOG \
                 (sudo setcap cap_syslog+ep $(command -v gpuviewer)) or set \
                 kernel.dmesg_restrict=0 to record Xid errors"
                    .into(),
            ),
            Err(e) => Err(format!("/dev/kmsg is not readable: {e}")),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("Xid errors are read from the Linux kernel log; not available on this OS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_modern_kmsg_record_with_pid_and_name() {
        let rec = "4,1234,5678901234,-;NVRM: Xid (PCI:0000:01:00): 79, pid=4521, name=python, GPU has fallen off the bus.\n SUBSYSTEM=pci\n";
        let x = parse_xid(rec).expect("an Xid line");
        assert_eq!(x.pci, "0000:01:00");
        assert_eq!(x.code, 79);
        assert_eq!(
            x.detail,
            "pid=4521, name=python, GPU has fallen off the bus"
        );
        assert!(
            x.raw.starts_with("NVRM: Xid"),
            "the kmsg header is not evidence"
        );
        assert!(
            !x.raw.contains("SUBSYSTEM"),
            "continuation lines are dropped"
        );
    }

    #[test]
    fn parses_a_bare_dmesg_line_and_an_older_driver_format() {
        let x = parse_xid(
            "[ 1234.5678] NVRM: Xid (PCI:0000:3B:00): 13, Graphics SM Warp Exception on (GPC 0, TPC 0): Out Of Range Address",
        )
        .expect("an Xid line");
        assert_eq!(x.pci, "0000:3b:00");
        assert_eq!(x.code, 13);
        assert!(
            x.detail.starts_with("Graphics SM Warp Exception"),
            "{}",
            x.detail
        );
    }

    #[test]
    fn non_xid_and_malformed_lines_are_none() {
        for l in [
            "6,1,2,-;usb 1-1: new high-speed USB device",
            "6,1,2,-;NVRM: loading NVIDIA UNIX x86_64 Kernel Module",
            "NVRM: Xid (PCI:0000:01:00): , no code",
            "NVRM: Xid (PCI:): 79, empty address",
            "",
        ] {
            assert_eq!(parse_xid(l), None, "{l:?}");
        }
    }

    #[test]
    fn xid_address_matches_the_device_regardless_of_domain_width_and_case() {
        assert!(xid_matches_device("0000:01:00", "00000000:01:00.0"));
        assert!(xid_matches_device("0000:3b:00", "0000:3B:00.0"));
        assert!(!xid_matches_device("0000:02:00", "0000:01:00.0"));
        assert!(
            !xid_matches_device("0000:01:00", "mock:0000:01:00.0"),
            "synthetic ids never match"
        );
    }
}
