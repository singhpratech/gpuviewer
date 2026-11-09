//! `gpuviewer doctor` — what this machine can and cannot record, and why.
//!
//! The AMD/Intel backends are fixture-tested but have seen little real silicon, so the
//! most useful bug report is "here is exactly what your build saw on my box". This
//! command produces that: every compiled-in backend (initialized, or the reason it was
//! skipped), every device with each metric marked available or unavailable from a real
//! two-tick probe, the process-list state and its hint, the Xid kernel-log status, and
//! the history database. Plain text, no ANSI, nothing recorded — it never opens the
//! history database for writing, so it cannot migrate, stamp, or lock it.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::Result;
use gpuviewer_core::{probe_backends, DynamicSample, StaticInfo, Vendor};
use gpuviewer_history::{DataSource, SqliteStore};

use crate::collector::{Engine, EngineConfig, FrameDevice};

/// Gap between the two probe ticks. Rate metrics (Intel energy→power, per-process CPU%,
/// fdinfo engine util) need two reads; one second matches the default interval.
const PROBE_GAP: Duration = Duration::from_secs(1);

/// Build the full report. `db` is the history database the live session would record
/// to (resolved by the caller with the same rules the recorder uses).
pub fn report(mock: bool, db: Option<std::path::PathBuf>) -> Result<String> {
    let mut out = String::new();
    let (backends, skipped) = probe_backends(mock);
    let names: Vec<&'static str> = backends.iter().map(|b| b.name()).collect();

    writeln!(
        out,
        "gpuviewer {} doctor — what this build can see on this machine\n",
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(out, "system")?;
    writeln!(out, "  os            {}", os_line())?;
    if let Some(note) = wsl_note() {
        writeln!(out, "  note          {note}")?;
    }

    writeln!(out, "\nbackends")?;
    for n in &names {
        let label = if *n == "mock" && !mock {
            "mock    in use — NO real backend initialized; everything below is SIMULATED"
        } else if *n == "mock" {
            "mock    in use (--mock)"
        } else {
            ""
        };
        if label.is_empty() {
            writeln!(out, "  {n:<8}initialized")?;
        } else {
            writeln!(out, "  {label}")?;
        }
    }
    for (n, why) in &skipped {
        writeln!(out, "  {n:<8}skipped — {why}")?;
    }

    let mut engine = Engine::with_backends(
        backends,
        EngineConfig {
            force_mock: mock,
            persist: false,
            ..Default::default()
        },
    );
    let infos = engine.static_infos();
    let _ = engine.tick();
    std::thread::sleep(PROBE_GAP);
    let frame = engine.tick();

    writeln!(out, "\ndevices")?;
    if infos.is_empty() {
        writeln!(out, "  none — no backend reported a device")?;
    }
    for (i, (info, fd)) in infos.iter().zip(frame.devices.iter()).enumerate() {
        device_section(&mut out, i, info, fd)?;
    }

    writeln!(out, "\nevent sources")?;
    let any_nvidia = !mock && infos.iter().any(|i| i.vendor == Vendor::Nvidia);
    let xid = if any_nvidia {
        match gpuviewer_core::kmsg::xid_watch_status() {
            Ok(()) => "enabled — NVIDIA Xid errors are read from the kernel log".to_string(),
            Err(why) => format!("off — {why}"),
        }
    } else {
        "not applicable — no real NVIDIA device".to_string()
    };
    writeln!(out, "  xid errors    {xid}")?;

    writeln!(out, "\nhistory")?;
    history_section(&mut out, db)?;
    Ok(out)
}

fn device_section(out: &mut String, i: usize, info: &StaticInfo, fd: &FrameDevice) -> Result<()> {
    writeln!(
        out,
        "  GPU{i}  {}  [{}]  via {}",
        info.name, info.id, info.backend
    )?;
    if let Some(v) = &info.driver_version {
        writeln!(out, "        driver        {v}")?;
    }
    if let Some(t) = info.mem_total_bytes {
        writeln!(
            out,
            "        memory total  {}",
            gpuviewer_core::fmt_bytes(t)
        )?;
    }
    match &fd.sample {
        Some(s) => {
            let (have, missing) = metric_split(s);
            writeln!(out, "        available     {}", list_or_none(&have))?;
            writeln!(out, "        unavailable   {}", list_or_none(&missing))?;
        }
        None => writeln!(
            out,
            "        sample        FAILED — the device did not answer its probe"
        )?,
    }
    let link_baseline = match (info.pcie_max_gen, info.pcie_max_width) {
        (Some(g), Some(w)) => format!(
            "Gen{g} x{w} as reported — the generation is judged under load, the width is not \
             (laptops wire x8 boards that report x16)"
        ),
        _ => "unknown — the degraded-link check is off for this device".into(),
    };
    writeln!(out, "        pcie max      {link_baseline}")?;
    match &fd.processes {
        Some(p) => writeln!(out, "        processes     {} visible", p.len())?,
        None => writeln!(out, "        processes     probe FAILED — list unknown")?,
    }
    if let Some(h) = &info.process_hint {
        writeln!(out, "        process note  {h}")?;
    }
    if let Some(c) = &info.source_caveat {
        writeln!(out, "        caveat        {c}")?;
    }
    Ok(())
}

/// Every per-tick metric, split by whether this source reported it.
fn metric_split(s: &DynamicSample) -> (Vec<&'static str>, Vec<&'static str>) {
    let fields: [(&'static str, bool); 15] = [
        ("util", s.util_pct.is_some()),
        ("vram used", s.mem_used_bytes.is_some()),
        ("power", s.power_mw.is_some()),
        ("power limit", s.power_limit_mw.is_some()),
        ("temp", s.temp_c.is_some()),
        ("fan", s.fan_pct.is_some()),
        ("sm clock", s.sm_clock_mhz.is_some()),
        ("mem clock", s.mem_clock_mhz.is_some()),
        ("encoder", s.encoder_pct.is_some()),
        ("decoder", s.decoder_pct.is_some()),
        ("throttle reasons", s.throttle.is_some()),
        ("pcie gen", s.pcie_gen.is_some()),
        ("pcie width", s.pcie_width.is_some()),
        ("ecc", s.ecc_uncorrected.is_some()),
        ("util engine", s.util_engine.is_some()),
    ];
    let mut have = Vec::new();
    let mut missing = Vec::new();
    for (n, ok) in fields {
        // `util_engine` is only meaningful where util is an engine headline (WDDM);
        // listing it as "unavailable" everywhere else would read as a defect.
        if n == "util engine" && !ok {
            continue;
        }
        if ok {
            have.push(n);
        } else {
            missing.push(n);
        }
    }
    (have, missing)
}

fn list_or_none(v: &[&str]) -> String {
    if v.is_empty() {
        "—".into()
    } else {
        v.join(", ")
    }
}

fn history_section(out: &mut String, db: Option<std::path::PathBuf>) -> Result<()> {
    let Some(path) = db else {
        writeln!(out, "  database      no data directory could be resolved")?;
        return Ok(());
    };
    writeln!(out, "  database      {}", path.display())?;
    if !path.exists() {
        writeln!(
            out,
            "  state         not created yet — the first live session creates it"
        )?;
        return Ok(());
    }
    if let Ok(md) = std::fs::metadata(&path) {
        let size = if md.len() < 1024 * 1024 {
            format!("{} KiB", md.len().div_ceil(1024))
        } else {
            gpuviewer_core::fmt_bytes(md.len())
        };
        writeln!(out, "  size          {size}")?;
    }
    match SqliteStore::open_readonly(&path) {
        Ok(store) => {
            if let Ok(v) = store.schema_version() {
                let note = if v > gpuviewer_history::store::SCHEMA_VERSION {
                    " (written by a NEWER gpuviewer — this build will not record into it)"
                } else {
                    ""
                };
                writeln!(out, "  schema        v{v}{note}")?;
            }
            let stamp = match store.data_source() {
                Ok(Some(DataSource::Real)) => "real",
                Ok(Some(DataSource::Mock)) => "mock (simulated)",
                _ => "unstamped",
            };
            writeln!(out, "  data source   {stamp}")?;
            if let (Ok(Some(from)), Ok(Some(to))) =
                (store.earliest_bucket_ms(), store.latest_bucket_ms())
            {
                writeln!(
                    out,
                    "  recorded      {} .. {}",
                    crate::fmt_clock(from),
                    crate::fmt_clock(to)
                )?;
            }
        }
        Err(e) => writeln!(out, "  state         unreadable: {e}")?,
    }
    Ok(())
}

fn os_line() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    #[cfg(target_os = "linux")]
    {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());
        format!("{os} {arch}, kernel {kernel}")
    }
    #[cfg(not(target_os = "linux"))]
    {
        format!("{os} {arch}")
    }
}

/// WSL2 limits per-process GPU data at the driver level; say so up front.
fn wsl_note() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
        rel.to_ascii_lowercase().contains("microsoft").then_some(
            "WSL2 detected — per-process GPU data is unavailable at the driver level here",
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
