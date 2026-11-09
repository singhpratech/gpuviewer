#![cfg(target_os = "linux")]
//! Intel backend fixture tests — run against the SYNTHETIC trees under `tests/fixtures/`
//! (see the README there). The two driver dialects are tested separately because they
//! disagree on everything that matters: fdinfo keys, utilization math (i915 busy-ns over
//! wall time vs xe busy-cycles over total-cycles), and sysfs freq layout. Mixing them up
//! is the classic Intel porting bug these tests exist to prevent.

use std::path::{Path, PathBuf};

use gpuviewer_core::intel::IntelBackend;
use gpuviewer_core::{BackendError, DeviceId, GpuBackend, ProcessKind, Vendor};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// ---- i915 dialect (Arc A770 dGPU, kernel 6.8 ABI) ----

#[test]
fn i915_enumeration_and_static_info() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:03:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.vendor, Vendor::Intel);
    assert_eq!(info.backend, "intel");
    // 8086:56a0 is in the well-known-id table.
    assert_eq!(info.name, "Intel Arc A770");
    // Dedicated VRAM from card-level lmem_total_bytes (16 GiB), bytes as-is.
    assert_eq!(info.mem_total_bytes, Some(17_179_869_184));
    // power1_max is MICROwatts: 190000000 µW must become 190000 mW (190 W).
    assert_eq!(info.power_limit_mw, Some(190_000));
    // gt_RP0_freq_mhz (2400) is the hardware max — NOT the gt_max_freq_mhz user cap,
    // which this fixture deliberately sets lower (2000) so reading the wrong file fails.
    assert_eq!(info.max_sm_clock_mhz, Some(2400));
    assert_eq!(info.driver_version, None);
    // The fixture root has no proc/self/status, so privilege is unknown → the backend
    // must claim incompleteness rather than overpromise.
    assert_eq!(
        info.process_hint.as_deref(),
        Some("showing your processes only — others need root or CAP_SYS_PTRACE (fdinfo)")
    );
}

#[test]
fn i915_dynamic_sample_prefers_actual_freq_and_stays_honest() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-kernel6.8")).unwrap();
    let dev = b.devices().remove(0);

    let s = b.refresh_dynamic(&dev).unwrap();
    // gt_act_freq_mhz (measured, 1850) wins over gt_cur_freq_mhz (requested, 2100).
    assert_eq!(s.sm_clock_mhz, Some(1850));
    // Device-level busyness needs the perf PMU (root/CAP_PERFMON): honest None.
    assert_eq!(s.util_pct, None);
    // No device-wide VRAM-used counter on Intel: honest None.
    assert_eq!(s.mem_used_bytes, None);
    // hwmon has no temp on kernel 6.8 (the i915 gate is 6.12+) and never a fan max.
    assert_eq!(s.temp_c, None);
    assert_eq!(s.fan_pct, None);
    // Power is derived from the cumulative energy counter: no baseline on the first
    // sighting → None, never a fabricated 0 W.
    assert_eq!(s.power_mw, None);
    assert_eq!(s.mem_clock_mhz, None);
    assert_eq!(s.encoder_pct, None);
    assert_eq!(s.decoder_pct, None);
    // The fixture's throttle_reason_status is 0 (quiescent GT): an OBSERVED
    // not-throttling — Some(all-false), even though every reason file is present.
    // status is the gate, and its readability is the observability gate (None would
    // mean "this kernel exposes no throttle interface").
    let t = s
        .throttle
        .expect("status file present → throttle observable");
    assert!(!t.any(), "quiescent GT (status=0) must report no throttle");
}

#[test]
fn i915_act_freq_absence_falls_back_to_cur_freq() {
    let scratch = std::env::temp_dir().join(format!("gpuviewer-i915-freq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);
    std::fs::remove_file(scratch.join("sys/class/drm/card1/gt_act_freq_mhz")).unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let s = b.refresh_dynamic(&dev).unwrap();
    assert_eq!(s.sm_clock_mhz, Some(2100));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn i915_act_freq_zero_means_gt_asleep_not_zero_mhz() {
    // act_freq reads 0 while the GT is power-gated in RC6 — that is "not clocked right
    // now", not a measured 0 MHz, and must surface as None (never as the requested
    // cur_freq either: the GT is asleep, claiming the requested clock would be a lie).
    let scratch = std::env::temp_dir().join(format!("gpuviewer-i915-rc6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);
    std::fs::write(scratch.join("sys/class/drm/card1/gt_act_freq_mhz"), "0\n").unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().sm_clock_mhz, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn i915_power_appears_on_second_energy_sighting() {
    // The energy1_input µJ counter only yields power as a delta between two sightings —
    // this pins the per-device watermark plumbing in refresh_dynamic (the exact µJ/ms
    // math is unit-tested; wall time here is real, so only Some-ness is asserted).
    let scratch =
        std::env::temp_dir().join(format!("gpuviewer-i915-energy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);
    let energy = scratch.join("sys/class/drm/card1/device/hwmon/hwmon5/energy1_input");

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, None);

    // Bump the cumulative counter by 6 J across a real wall delta: power must appear.
    std::thread::sleep(std::time::Duration::from_millis(60));
    let cur: u64 = std::fs::read_to_string(&energy)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::write(&energy, format!("{}\n", cur + 6_000_000)).unwrap();
    let power = b.refresh_dynamic(&dev).unwrap().power_mw;
    assert!(
        power.is_some_and(|mw| mw > 0),
        "second energy sighting must derive a positive power, got {power:?}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn i915_fdinfo_processes() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-kernel6.8")).unwrap();
    let dev = b.devices().remove(0);

    let procs = b.refresh_processes(&dev).unwrap();
    assert_eq!(
        procs.len(),
        4,
        "the pid on the other GPU's pdev must be filtered out"
    );

    let ffplay = &procs[0];
    assert_eq!((ffplay.pid, ffplay.name.as_str()), (3100, "ffplay"));
    // drm-engine-video > 0 ns is decisively media.
    assert_eq!(ffplay.kind, ProcessKind::Graphics);
    // Max across the pid's two fds: 786432 KiB → 805306368 bytes — never the 4096 KiB
    // fd, and never a sum of the two.
    assert_eq!(ffplay.mem_bytes, Some(805_306_368));

    let blender = &procs[1];
    assert_eq!((blender.pid, blender.name.as_str()), (5200, "blender"));
    // Render-only could be 3D or pre-CCS GPGPU — Unknown, not a guess.
    assert_eq!(blender.kind, ProcessKind::Unknown);
    assert_eq!(blender.mem_bytes, Some(2_147_483_648));

    let py = &procs[2];
    assert_eq!((py.pid, py.name.as_str()), (6300, "python3"));
    // drm-engine-compute > 0 ns is decisive.
    assert_eq!(py.kind, ProcessKind::Compute);
    assert_eq!(py.mem_bytes, Some(6_442_450_944));

    // The pre-gate pid: memory regions but NO drm-engine-* keys at all. It is still
    // listed (its memory is real), with honestly absent engine columns: Unknown kind,
    // util None — never a 0% invented from missing keys.
    let kwin = &procs[3];
    assert_eq!((kwin.pid, kwin.name.as_str()), (7400, "kwin_wayland"));
    assert_eq!(kwin.kind, ProcessKind::Unknown);
    // drm-total-local0 (64 MiB) — the system0 region beside it is not VRAM.
    assert_eq!(kwin.mem_bytes, Some(67_108_864));

    // First sighting has no engine-ns baseline.
    assert!(procs.iter().all(|p| p.util_pct.is_none()));
}

#[test]
fn i915_util_is_render_ns_delta_over_wall_time() {
    // Watermark plumbing needs a counter that moves, so this test works on a scratch
    // copy of the fixture rather than the committed tree.
    let scratch =
        std::env::temp_dir().join(format!("gpuviewer-i915-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let first = b.refresh_processes(&dev).unwrap();
    assert!(first.iter().all(|p| p.util_pct.is_none()));

    // Advance ffplay's cumulative render-ns far past 100% of any plausible wall delta:
    // utilization must clamp, proving delta/wall (not the raw counter) is reported.
    std::thread::sleep(std::time::Duration::from_millis(60));
    let fd = scratch.join("proc/3100/fdinfo/7");
    let bumped = std::fs::read_to_string(&fd).unwrap().replace(
        "drm-engine-render:\t4500000000 ns",
        "drm-engine-render:\t901500000000 ns",
    );
    std::fs::write(&fd, bumped).unwrap();

    let second = b.refresh_processes(&dev).unwrap();
    let ffplay = second.iter().find(|p| p.pid == 3100).unwrap();
    assert_eq!(ffplay.util_pct, Some(100.0));
    // blender's counter did not move: 0 busy-ns over the same wall = 0%.
    let blender = second.iter().find(|p| p.pid == 5200).unwrap();
    assert_eq!(blender.util_pct, Some(0.0));
    // The pre-gate pid has no engine keys: still None on the second sighting — absence
    // of a counter is never a measured 0%.
    let kwin = second.iter().find(|p| p.pid == 7400).unwrap();
    assert_eq!(kwin.util_pct, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- xe dialect (Arc B580 dGPU, kernel 6.11 ABI) ----

#[test]
fn xe_enumeration_and_static_info() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-kernel6.11")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:03:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.vendor, Vendor::Intel);
    assert_eq!(info.name, "Intel Arc B580");
    // xe exposes no VRAM-total sysfs — None is honest even on a dGPU with VRAM.
    assert_eq!(info.mem_total_bytes, None);
    assert_eq!(info.power_limit_mw, Some(190_000));
    // tile0/gt0/freq0/rp0_freq (2850) is the hardware max — NOT the max_freq user cap,
    // which this fixture deliberately sets lower (2600) so reading the wrong file fails.
    assert_eq!(info.max_sm_clock_mhz, Some(2850));
    assert_eq!(
        info.process_hint.as_deref(),
        Some("showing your processes only — others need root or CAP_SYS_PTRACE (fdinfo)")
    );
}

#[test]
fn xe_dynamic_sample_reads_tile_gt_freq() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-kernel6.11")).unwrap();
    let dev = b.devices().remove(0);

    let s = b.refresh_dynamic(&dev).unwrap();
    // tile0/gt0/freq0/act_freq (measured, 2400) wins over cur_freq (requested, 2700).
    assert_eq!(s.sm_clock_mhz, Some(2400));
    assert_eq!(s.util_pct, None);
    assert_eq!(s.mem_used_bytes, None);
    // xe hwmon has no temp until 6.15 and no fan until 6.16 — None is the 6.11 truth.
    assert_eq!(s.temp_c, None);
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.power_mw, None); // first energy sighting has no baseline
                                  // freq0/throttle/status is 0 in the fixture: quiescent —
                                  // an observed Some(all-false), not an unobservable None.
    assert!(!s.throttle.expect("status present → observable").any());
}

#[test]
fn xe_fdinfo_processes() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-kernel6.11")).unwrap();
    let dev = b.devices().remove(0);

    let procs = b.refresh_processes(&dev).unwrap();
    assert_eq!(procs.len(), 2);

    let shell = &procs[0];
    assert_eq!((shell.pid, shell.name.as_str()), (2600, "gnome-shell"));
    // rcs-only activity (vcs and ccs both 0 cycles): honestly Unknown, not a guess.
    assert_eq!(shell.kind, ProcessKind::Unknown);
    assert_eq!(shell.mem_bytes, Some(536_870_912)); // drm-total-vram0: 524288 KiB

    let py = &procs[1];
    assert_eq!((py.pid, py.name.as_str()), (4400, "python3"));
    // drm-cycles-ccs > 0 is decisive.
    assert_eq!(py.kind, ProcessKind::Compute);
    // drm-total-vram0 (2097152 KiB) wins over drm-resident-vram0 (1048576 KiB).
    assert_eq!(py.mem_bytes, Some(2_147_483_648));

    // First sighting has no cycle baseline.
    assert!(procs.iter().all(|p| p.util_pct.is_none()));
}

#[test]
fn xe_util_is_cycles_delta_over_total_cycles_not_wall_time() {
    let scratch = std::env::temp_dir().join(format!("gpuviewer-xe-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-xe-kernel6.11"), &scratch);

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let first = b.refresh_processes(&dev).unwrap();
    assert!(first.iter().all(|p| p.util_pct.is_none()));

    // Advance python3 by hand-picked deltas: +600,000 busy cycles over +1,200,000
    // total cycles = exactly 50%. The second scan happens milliseconds after the
    // first, so an implementation that wrongly divided by wall time (the i915 math —
    // the classic xe porting bug) would clamp to 100% here, not produce 50%.
    let fd = scratch.join("proc/4400/fdinfo/9");
    let bumped = std::fs::read_to_string(&fd)
        .unwrap()
        .replace("drm-cycles-rcs:\t1000000", "drm-cycles-rcs:\t1600000")
        .replace(
            "drm-total-cycles-rcs:\t50000000",
            "drm-total-cycles-rcs:\t51200000",
        );
    std::fs::write(&fd, bumped).unwrap();
    // gnome-shell's base advances but its busy cycles do not: exactly 0%.
    let fd = scratch.join("proc/2600/fdinfo/6");
    let bumped = std::fs::read_to_string(&fd).unwrap().replace(
        "drm-total-cycles-rcs:\t50000000",
        "drm-total-cycles-rcs:\t51200000",
    );
    std::fs::write(&fd, bumped).unwrap();

    let second = b.refresh_processes(&dev).unwrap();
    let py = second.iter().find(|p| p.pid == 4400).unwrap();
    assert_eq!(py.util_pct, Some(50.0));
    let shell = second.iter().find(|p| p.pid == 2600).unwrap();
    assert_eq!(shell.util_pct, Some(0.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- hwmon temp dialect (i915 temp1_input vs xe temp2_input — research 07 §3.2) ----
//
// The package sensor lives on a DIFFERENT hwmon channel per driver: i915 publishes it
// as temp1_input (gate 6.12+); xe has NO temp1 — its pkg temp is temp2_input and temp3
// is the VRAM sensor (gate 6.15+). Each tree below plants DECOYS on the channels the
// dialect must not read, so a wrong-channel read produces a recognizably wrong number
// instead of silently passing.

#[test]
fn i915_temp_is_temp1_never_the_temp2_decoy() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-kernel6.12-arc")).unwrap();
    let dev = b.devices().remove(0);

    let s = b.refresh_dynamic(&dev).unwrap();
    // temp1_input (61000 milli-°C) is the i915 package sensor; 53.0 would mean the
    // temp2_input decoy was read on the wrong dialect's channel.
    assert_eq!(s.temp_c, Some(61.0));
    // fan1_input (RPM) is present, but there is no fan1_max reference and the model
    // carries percent-of-max: no honest value exists.
    assert_eq!(s.fan_pct, None);
    // Card-level gt_act_freq_mhz (1850), never the per-GT rps_act_freq_mhz decoy (1500).
    assert_eq!(s.sm_clock_mhz, Some(1850));
}

#[test]
fn xe_temp_is_temp2_never_the_temp1_or_temp3_decoys() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-kernel6.15-bmg")).unwrap();
    let dev = b.devices().remove(0);

    let s = b.refresh_dynamic(&dev).unwrap();
    // temp2_input (58000 milli-°C) is the xe package sensor. 47.0 would mean the
    // temp1_input decoy was read (THE historical bug: i915's channel applied to xe —
    // real xe hwmon exposes no temp1 at all); 64.0 would mean temp3_input, the VRAM
    // sensor — a different physical claim that must never appear as device temp.
    assert_eq!(s.temp_c, Some(58.0));
    assert_eq!(s.fan_pct, None); // fan1_input RPM-only, no max → no honest percent
                                 // freq0/act_freq (2400), never the rpa_freq decoy (2200).
    assert_eq!(s.sm_clock_mhz, Some(2400));
    // Channel-choice decoys: the card channel power1_max (190 W) stays preferred over
    // the power2_max pkg decoy (220 W).
    let info = b.static_info(&dev).unwrap();
    assert_eq!(info.power_limit_mw, Some(190_000));
}

#[test]
fn xe_missing_temp2_is_none_never_a_fallback_channel() {
    // Remove the designated xe channel: temp1 (decoy) and temp3 (VRAM) remain, and
    // neither may be surfaced as device temp — the documented channel being absent
    // means the package temp is unobservable, not an invitation to read some other
    // sensor and present it as GPU temp.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-xe-temp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-xe-kernel6.15-bmg"), &scratch);
    std::fs::remove_file(scratch.join("sys/class/drm/card0/device/hwmon/hwmon6/temp2_input"))
        .unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().temp_c, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn i915_missing_temp1_is_none_never_a_fallback_channel() {
    // The mirror case: an i915 tree whose temp1_input vanished must not fall back to
    // the temp2_input decoy.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-i915-temp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.12-arc"), &scratch);
    std::fs::remove_file(scratch.join("sys/class/drm/card1/device/hwmon/hwmon5/temp1_input"))
        .unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().temp_c, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- throttle_reason decoding (i915 gt/gt0/throttle_reason_*, xe freq0/throttle/*) ----
//
// The fixtures ship quiescent (status=0); these scratch copies drive the bit→model
// mapping and, above all, the rule that `status` is the GATE: a reason file is only
// honored when status==1.

/// One i915 throttle flag file: `{card}/gt/gt0/throttle_reason_<name>`.
fn i915_throttle(scratch: &Path, name: &str) -> PathBuf {
    scratch.join(format!("sys/class/drm/card1/gt/gt0/throttle_reason_{name}"))
}

/// One xe throttle flag file: `{dev}/tile0/gt0/freq0/throttle/<name>`.
fn xe_throttle(scratch: &Path, name: &str) -> PathBuf {
    scratch.join(format!(
        "sys/class/drm/card0/device/tile0/gt0/freq0/throttle/{name}"
    ))
}

#[test]
fn i915_throttle_thermal_then_thermal_plus_power_cap() {
    let scratch = std::env::temp_dir().join(format!("gpuviewer-i915-thr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);

    // status=1 + thermal=1 → thermal only, NOT power_cap.
    std::fs::write(i915_throttle(&scratch, "status"), "1\n").unwrap();
    std::fs::write(i915_throttle(&scratch, "thermal"), "1\n").unwrap();
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=1 → observable");
    assert!(t.thermal, "thermal=1 must map to throttle.thermal");
    assert!(!t.power_cap, "no pl* bit set → power_cap must stay false");
    assert!(!t.hw_slowdown && !t.other);

    // Add pl1=1 → both thermal AND power_cap (pl1|pl2|pl4 → power_cap).
    std::fs::write(i915_throttle(&scratch, "pl1"), "1\n").unwrap();
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=1 → observable");
    assert!(t.thermal && t.power_cap, "thermal + pl1 → both");

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_throttle_pl1_is_power_cap() {
    // THE xe test — nobody else ships throttle decoding on xe (intel_gpu_top does not).
    // status=1 + reason_pl1=1 → power_cap, proving the xe filename spelling (reason_pl1,
    // not throttle_reason_pl1) and the freq0/throttle/ path are both read correctly.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-xe-thr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-xe-kernel6.11"), &scratch);

    std::fs::write(xe_throttle(&scratch, "status"), "1\n").unwrap();
    std::fs::write(xe_throttle(&scratch, "reason_pl1"), "1\n").unwrap();
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=1 → observable");
    assert!(t.power_cap, "reason_pl1=1 must map to throttle.power_cap");
    assert!(!t.thermal && !t.hw_slowdown && !t.other);

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_throttle_ratl_and_vr_map_to_other() {
    // RATL and the VR alerts have no closer model bucket than `other`; they ARE real
    // limits and must not be silently dropped.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-xe-other-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-xe-kernel6.11"), &scratch);

    std::fs::write(xe_throttle(&scratch, "status"), "1\n").unwrap();
    std::fs::write(xe_throttle(&scratch, "reason_vr_tdc"), "1\n").unwrap();
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=1 → observable");
    assert!(t.other, "vr_tdc=1 must map to throttle.other");
    assert!(!t.thermal && !t.power_cap && !t.hw_slowdown);

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn throttle_status_zero_ignores_stale_reason_files() {
    // status=0 but thermal=1 (a stale latched reason bit): default, nothing claimed —
    // trusting the reason file here would narrate a throttle that is not happening.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-i915-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);

    // status stays 0 (quiescent fixture default); set a reason file to 1 anyway.
    std::fs::write(i915_throttle(&scratch, "thermal"), "1\n").unwrap();
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=0 is still an observation");
    assert!(
        !t.any(),
        "status=0 gates out a stale reason file: {:?}",
        t.labels()
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn throttle_status_one_with_no_reason_files_is_other_only() {
    // status=1 but the kernel exposed no reason files at all (or none we recognize):
    // something is throttling, cause unknown — assert the honest minimum (`other`),
    // never swallow the signal.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-xe-cause-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-xe-kernel6.11"), &scratch);

    std::fs::write(xe_throttle(&scratch, "status"), "1\n").unwrap();
    // Remove every reason file: only `status` remains.
    for name in [
        "reason_pl1",
        "reason_pl2",
        "reason_pl4",
        "reason_thermal",
        "reason_prochot",
        "reason_ratl",
        "reason_vr_thermalert",
        "reason_vr_tdc",
    ] {
        std::fs::remove_file(xe_throttle(&scratch, name)).unwrap();
    }
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=1 → observable");
    assert_eq!(
        (t.thermal, t.power_cap, t.hw_slowdown, t.other),
        (false, false, false, true),
        "status=1 with no recognized reason → other only"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn throttle_garbage_status_does_not_panic_and_claims_nothing() {
    // A truncated/garbage status file ("zz") is a broken interface: unobservable →
    // None (§5.4), no panic. Even with a reason file set to 1, the gate keeps us
    // silent — and None is silence, not an asserted "not throttling".
    let scratch =
        std::env::temp_dir().join(format!("gpuviewer-i915-garbage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("intel-i915-kernel6.8"), &scratch);

    std::fs::write(i915_throttle(&scratch, "status"), "zz").unwrap();
    std::fs::write(i915_throttle(&scratch, "pl2"), "1\n").unwrap();
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b.refresh_dynamic(&dev).unwrap().throttle;
    assert_eq!(
        t, None,
        "garbage status is unobservable, not observed-quiet"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- xe power channel: card preferred, pkg fallback, one coherent channel (§3.2.2) ----
//
// xe numbers hwmon power channels by scope: index 1 = card, index 2 = pkg. DG2 forced
// onto xe exposes pkg only, so a card-only read reports None while a real Some sits in
// sysfs. The channel is resolved ONCE per device and shared by the limit and the draw:
// pkg draw over a card limit would be a cross-scope ratio on the power gauge.

const PKG_CAVEAT: &str = "power: xe pkg (GPU package) channel, not whole-card power";

/// The B580's hwmon dir in the `intel-xe-kernel6.15-bmg` tree.
const BMG_HWMON: &str = "sys/class/drm/card0/device/hwmon/hwmon6";

/// The A770-on-xe's hwmon dir in the `intel-xe-dg2-forced` tree.
const DG2_HWMON: &str = "sys/class/drm/card0/device/hwmon/hwmon3";

#[test]
fn xe_dg2_forced_static_info_falls_back_to_pkg_channel() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-dg2-forced")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:03:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    // The name table is keyed on the PCI id, not the driver: still an A770 on xe.
    assert_eq!(info.name, "Intel Arc A770");
    // xe has no VRAM-total sysfs even for a part the i915 tree reports 16 GiB on.
    assert_eq!(info.mem_total_bytes, None);
    // power2_max (225 W, the pkg PL1) — a card-only read would give None, and the
    // power2_rated_max decoy (190 W, the default TDP) is not the enforced limit.
    assert_eq!(info.power_limit_mw, Some(225_000));
    // rp0_freq (2400), never the max_freq user cap (2200).
    assert_eq!(info.max_sm_clock_mhz, Some(2400));
    // The pkg-scope reading is labeled where the model has a slot for it.
    assert_eq!(info.source_caveat.as_deref(), Some(PKG_CAVEAT));
}

#[test]
fn xe_dg2_forced_power_derives_from_pkg_energy() {
    let scratch = scratch_copy("xe-dg2-power", "intel-xe-dg2-forced");
    let energy = scratch.join(DG2_HWMON).join("energy2_input");

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let s = b.refresh_dynamic(&dev).unwrap();
    assert_eq!(s.power_mw, None, "first energy sighting has no baseline");
    assert_eq!(s.sm_clock_mhz, Some(2000));
    // No temp files in this tree (xe temps gate 6.15+): None, no fallback.
    assert_eq!(s.temp_c, None);
    assert!(!s.throttle.expect("status present → observable").any());

    std::thread::sleep(std::time::Duration::from_millis(60));
    bump_u64(&energy, 6_000_000);
    let power = b.refresh_dynamic(&dev).unwrap().power_mw;
    assert!(
        power.is_some_and(|mw| mw > 0),
        "pkg energy2_input must derive power on a pkg-only device, got {power:?}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_dg2_forced_processes_and_cycles_util() {
    let scratch = scratch_copy("xe-dg2-procs", "intel-xe-dg2-forced");
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);

    let first = b.refresh_processes(&dev).unwrap();
    assert_eq!(first.len(), 1);
    let py = &first[0];
    assert_eq!((py.pid, py.name.as_str()), (5100, "python3"));
    // drm-cycles-ccs > 0 is decisive.
    assert_eq!(py.kind, ProcessKind::Compute);
    // drm-total-vram0 (4 GiB) over resident (3 GiB); gtt is not VRAM.
    assert_eq!(py.mem_bytes, Some(4_294_967_296));
    assert_eq!(py.util_pct, None);

    // +600,000 rcs cycles over +1,200,000 total, capacity-rcs 1 → exactly 50%.
    let fd = scratch.join("proc/5100/fdinfo/12");
    let bumped = std::fs::read_to_string(&fd)
        .unwrap()
        .replace("drm-cycles-rcs:\t300000", "drm-cycles-rcs:\t900000")
        .replace(
            "drm-total-cycles-rcs:\t80000000",
            "drm-total-cycles-rcs:\t81200000",
        );
    std::fs::write(&fd, bumped).unwrap();
    let second = b.refresh_processes(&dev).unwrap();
    assert_eq!(second[0].util_pct, Some(50.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_bmg_power_draw_reads_card_energy_not_pkg_decoy() {
    // Both channels present: card wins. Advance ONLY the pkg counter — a draw read from
    // the energy2_input decoy would be large; the card counter did not move, so the
    // honest derived draw is exactly 0 mW.
    let scratch = scratch_copy("xe-bmg-card", "intel-xe-kernel6.15-bmg");
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.static_info(&dev).unwrap().source_caveat, None);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, None);

    std::thread::sleep(std::time::Duration::from_millis(60));
    bump_u64(&scratch.join(BMG_HWMON).join("energy2_input"), 50_000_000);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, Some(0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_bmg_without_card_energy_uses_a_coherent_pkg_pair() {
    // Card energy is PMT-gated on BMG: with energy1_input gone, power comes from pkg —
    // and the LIMIT must come from pkg too (power2_max 220 W), never the card's
    // power1_max (190 W): pkg draw over a card limit is a cross-scope ratio.
    let scratch = scratch_copy("xe-bmg-pkg", "intel-xe-kernel6.15-bmg");
    let hw = scratch.join(BMG_HWMON);
    std::fs::remove_file(hw.join("energy1_input")).unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let info = b.static_info(&dev).unwrap();
    assert_eq!(info.power_limit_mw, Some(220_000));
    assert_eq!(info.source_caveat.as_deref(), Some(PKG_CAVEAT));
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, None);

    std::thread::sleep(std::time::Duration::from_millis(60));
    bump_u64(&hw.join("energy2_input"), 6_000_000);
    assert!(b
        .refresh_dynamic(&dev)
        .unwrap()
        .power_mw
        .is_some_and(|mw| mw > 0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_card_limit_without_any_energy_stays_card_scoped() {
    // Neither channel has an energy counter: the limit files decide, card first. The
    // draw is honestly None (nothing to derive it from), and no pkg label applies.
    let scratch = scratch_copy("xe-bmg-noenergy", "intel-xe-kernel6.15-bmg");
    let hw = scratch.join(BMG_HWMON);
    std::fs::remove_file(hw.join("energy1_input")).unwrap();
    std::fs::remove_file(hw.join("energy2_input")).unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let info = b.static_info(&dev).unwrap();
    assert_eq!(info.power_limit_mw, Some(190_000));
    assert_eq!(info.source_caveat, None);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_no_power_channel_at_all_is_none_everywhere() {
    let scratch = scratch_copy("xe-bmg-nopower", "intel-xe-kernel6.15-bmg");
    let hw = scratch.join(BMG_HWMON);
    for f in ["energy1_input", "energy2_input", "power1_max", "power2_max"] {
        std::fs::remove_file(hw.join(f)).unwrap();
    }

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let info = b.static_info(&dev).unwrap();
    assert_eq!(info.power_limit_mw, None);
    assert_eq!(info.source_caveat, None);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, None);
    // Temp is a separate channel family and is unaffected.
    assert_eq!(b.refresh_dynamic(&dev).unwrap().temp_c, Some(58.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn i915_never_falls_back_to_an_index2_power_channel() {
    // The pkg fallback is xe-only: i915 hwmon has no index-2 power channel, so stray
    // power2_max/energy2_input files on an i915 tree must never be read as its power.
    let scratch = scratch_copy("i915-noidx2", "intel-i915-kernel6.8");
    let hw = scratch.join("sys/class/drm/card1/device/hwmon/hwmon5");
    std::fs::remove_file(hw.join("power1_max")).unwrap();
    std::fs::remove_file(hw.join("energy1_input")).unwrap();
    std::fs::write(hw.join("power2_max"), "150000000\n").unwrap();
    std::fs::write(hw.join("energy2_input"), "9000000000\n").unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let info = b.static_info(&dev).unwrap();
    assert_eq!(info.power_limit_mw, None);
    assert_eq!(info.source_caveat, None);
    b.refresh_dynamic(&dev).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    bump_u64(&hw.join("energy2_input"), 6_000_000);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- iGPU trees: i915 Meteor Lake, xe Lunar Lake ----
//
// The iGPU half of the install base: no hwmon, no VRAM, system-memory-only fdinfo
// regions (which must NEVER surface as VRAM), and a second (media) GT whose throttle
// and freq files are DECOYS — only gt0 tells the render device's story.

#[test]
fn mtl_igpu_static_info_is_honest_about_absences() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-igpu-mtl")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:00:02.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "Intel GPU [8086:7d55]"); // not a known Arc dGPU id
    assert_eq!(info.mem_total_bytes, None); // no lmem_total_bytes: shares system RAM
    assert_eq!(info.power_limit_mw, None); // no hwmon on an iGPU
    assert_eq!(info.source_caveat, None);
    // Card-level gt_RP0_freq_mhz (2250) — not gt1's rps_RP0 decoy (1300), not the
    // gt_max_freq_mhz user cap (2000).
    assert_eq!(info.max_sm_clock_mhz, Some(2250));
}

#[test]
fn mtl_igpu_dynamic_ignores_the_media_gt1_decoys() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-igpu-mtl")).unwrap();
    let dev = b.devices().remove(0);

    let s = b.refresh_dynamic(&dev).unwrap();
    assert_eq!(s.sm_clock_mhz, Some(1650)); // card-level act, not gt1 rps_act (1100)
    assert_eq!(s.temp_c, None);
    assert_eq!(s.power_mw, None);
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.util_pct, None);
    assert_eq!(s.mem_used_bytes, None);
    // gt1 (the media GT) has status=1 with thermal+pl1 set; gt0 is quiescent. Reading
    // gt1 would narrate a render-device throttle that is not happening.
    let t = s.throttle.expect("gt0 status present → observable");
    assert!(!t.any(), "gt1's throttle must not leak: {:?}", t.labels());
}

#[test]
fn mtl_igpu_gt0_throttle_is_not_contaminated_by_gt1() {
    // gt0 genuinely throttling on pl1 alone: power_cap, and NOT gt1's thermal decoy.
    let scratch = scratch_copy("mtl-thr", "intel-i915-igpu-mtl");
    let gt0 = scratch.join("sys/class/drm/card0/gt/gt0");
    std::fs::write(gt0.join("throttle_reason_status"), "1\n").unwrap();
    std::fs::write(gt0.join("throttle_reason_pl1"), "1\n").unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let t = b
        .refresh_dynamic(&dev)
        .unwrap()
        .throttle
        .expect("status=1 → observable");
    assert!(t.power_cap);
    assert!(
        !t.thermal,
        "thermal is set only on gt1 (media GT) — a decoy"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn mtl_igpu_processes_system0_is_never_vram() {
    let mut b = IntelBackend::with_root(fixture("intel-i915-igpu-mtl")).unwrap();
    let dev = b.devices().remove(0);

    let procs = b.refresh_processes(&dev).unwrap();
    assert_eq!(procs.len(), 2);

    let shell = &procs[0];
    assert_eq!((shell.pid, shell.name.as_str()), (2900, "gnome-shell"));
    // Render-only: Unknown, not a guess.
    assert_eq!(shell.kind, ProcessKind::Unknown);
    // Only system0 / stolen-system0 regions: system RAM must not masquerade as VRAM.
    assert_eq!(shell.mem_bytes, None);

    let ff = &procs[1];
    assert_eq!((ff.pid, ff.name.as_str()), (4700, "ffmpeg"));
    assert_eq!(ff.kind, ProcessKind::Graphics); // busy video engine
    assert_eq!(ff.mem_bytes, None);

    assert!(procs.iter().all(|p| p.util_pct.is_none()));
}

#[test]
fn mtl_igpu_per_process_util_works_on_an_igpu() {
    let scratch = scratch_copy("mtl-util", "intel-i915-igpu-mtl");
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert!(b
        .refresh_processes(&dev)
        .unwrap()
        .iter()
        .all(|p| p.util_pct.is_none()));

    std::thread::sleep(std::time::Duration::from_millis(60));
    let fd = scratch.join("proc/2900/fdinfo/9");
    let bumped = std::fs::read_to_string(&fd).unwrap().replace(
        "drm-engine-render:\t2600000000 ns",
        "drm-engine-render:\t902600000000 ns",
    );
    std::fs::write(&fd, bumped).unwrap();

    let second = b.refresh_processes(&dev).unwrap();
    let shell = second.iter().find(|p| p.pid == 2900).unwrap();
    assert_eq!(shell.util_pct, Some(100.0));
    // ffmpeg's render counter did not move (its video capacity of 2 is a different
    // class's denominator and must not touch render util): exactly 0%.
    let ff = second.iter().find(|p| p.pid == 4700).unwrap();
    assert_eq!(ff.util_pct, Some(0.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn lnl_igpu_static_and_dynamic_read_gt0_only() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-igpu-lnl")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:00:02.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "Intel GPU [8086:64a0]");
    assert_eq!(info.mem_total_bytes, None);
    assert_eq!(info.power_limit_mw, None); // no hwmon: xe registers it for dGPUs only
    assert_eq!(info.source_caveat, None);
    // gt0 rp0_freq (2050) — not gt1's (1300), not the gt0 max_freq user cap (1900).
    assert_eq!(info.max_sm_clock_mhz, Some(2050));

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.sm_clock_mhz, Some(1450)); // gt0 act, never the media gt1 act (900)
    assert_eq!(s.temp_c, None);
    assert_eq!(s.power_mw, None);
    assert_eq!(s.fan_pct, None);
    let t = s.throttle.expect("gt0 status present → observable");
    assert!(
        !t.any(),
        "gt1's thermal throttle must not leak: {:?}",
        t.labels()
    );
}

#[test]
fn lnl_igpu_processes_have_no_vram0() {
    let mut b = IntelBackend::with_root(fixture("intel-xe-igpu-lnl")).unwrap();
    let dev = b.devices().remove(0);

    let procs = b.refresh_processes(&dev).unwrap();
    assert_eq!(procs.len(), 2);

    let shell = &procs[0];
    assert_eq!((shell.pid, shell.name.as_str()), (3300, "gnome-shell"));
    assert_eq!(shell.kind, ProcessKind::Unknown);
    // system / gtt / stolen regions only: none of them is VRAM.
    assert_eq!(shell.mem_bytes, None);

    let mpv = &procs[1];
    assert_eq!((mpv.pid, mpv.name.as_str()), (6100, "mpv"));
    assert_eq!(mpv.kind, ProcessKind::Graphics); // busy vcs
    assert_eq!(mpv.mem_bytes, None);

    assert!(procs.iter().all(|p| p.util_pct.is_none()));
}

#[test]
fn lnl_igpu_util_is_cycles_over_total_cycles() {
    let scratch = scratch_copy("lnl-util", "intel-xe-igpu-lnl");
    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    b.refresh_processes(&dev).unwrap();

    // gnome-shell: +450,000 busy over +900,000 total = exactly 50% (milliseconds of
    // wall time pass — a wall-time divisor would clamp to 100%).
    let fd = scratch.join("proc/3300/fdinfo/8");
    let bumped = std::fs::read_to_string(&fd)
        .unwrap()
        .replace("drm-cycles-rcs:\t7000000", "drm-cycles-rcs:\t7450000")
        .replace(
            "drm-total-cycles-rcs:\t90000000",
            "drm-total-cycles-rcs:\t90900000",
        );
    std::fs::write(&fd, bumped).unwrap();
    // mpv: base advances, rcs busy does not → exactly 0%.
    let fd = scratch.join("proc/6100/fdinfo/14");
    let bumped = std::fs::read_to_string(&fd).unwrap().replace(
        "drm-total-cycles-rcs:\t90000000",
        "drm-total-cycles-rcs:\t90900000",
    );
    std::fs::write(&fd, bumped).unwrap();

    let second = b.refresh_processes(&dev).unwrap();
    assert_eq!(
        second.iter().find(|p| p.pid == 3300).unwrap().util_pct,
        Some(50.0)
    );
    assert_eq!(
        second.iter().find(|p| p.pid == 6100).unwrap().util_pct,
        Some(0.0)
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- hardening: numeric hwmon sort, engine capacity, Arc id table (§3.2 minor) ----

#[test]
fn hwmon_sort_is_numeric_not_lexicographic() {
    // hwmon5 is the device's real hwmon; a second dir named hwmon10 carries a decoy
    // limit. A string sort puts "hwmon10" before "hwmon5" and would read 50 W.
    let scratch = scratch_copy("i915-hwmon-sort", "intel-i915-kernel6.8");
    let hw = scratch.join("sys/class/drm/card1/device/hwmon");
    copy_tree(&hw.join("hwmon5"), &hw.join("hwmon10"));
    std::fs::write(hw.join("hwmon10/power1_max"), "50000000\n").unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.static_info(&dev).unwrap().power_limit_mw, Some(190_000));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn xe_engine_capacity_divides_instead_of_clamping() {
    // Same +600,000 / +1,200,000 deltas as the 50% test, but the rcs class now reports
    // capacity 2: busy-cycles are summed across two instances, so the honest figure is
    // 25% — not 50%, and not a clamp.
    let scratch = scratch_copy("xe-capacity", "intel-xe-kernel6.11");
    let fd = scratch.join("proc/4400/fdinfo/9");
    let two = std::fs::read_to_string(&fd)
        .unwrap()
        .replace("drm-engine-capacity-rcs:\t1", "drm-engine-capacity-rcs:\t2");
    std::fs::write(&fd, two).unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    b.refresh_processes(&dev).unwrap();
    let bumped = std::fs::read_to_string(&fd)
        .unwrap()
        .replace("drm-cycles-rcs:\t1000000", "drm-cycles-rcs:\t1600000")
        .replace(
            "drm-total-cycles-rcs:\t50000000",
            "drm-total-cycles-rcs:\t51200000",
        );
    std::fs::write(&fd, bumped).unwrap();
    let second = b.refresh_processes(&dev).unwrap();
    assert_eq!(
        second.iter().find(|p| p.pid == 4400).unwrap().util_pct,
        Some(25.0)
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn i915_zero_engine_capacity_refuses_util() {
    // The spec forbids a zero capacity: dividing by it is impossible and substituting
    // the absent-key default (1) would be a guess — util is refused (None), while the
    // process stays listed and other pids are unaffected.
    let scratch = scratch_copy("i915-capacity0", "intel-i915-kernel6.8");
    let fd = scratch.join("proc/3100/fdinfo/7");
    let mut s = std::fs::read_to_string(&fd).unwrap();
    s.push_str("drm-engine-capacity-render:\t0\n");
    std::fs::write(&fd, s).unwrap();

    let mut b = IntelBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    b.refresh_processes(&dev).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    let second = b.refresh_processes(&dev).unwrap();
    assert_eq!(
        second.iter().find(|p| p.pid == 3100).unwrap().util_pct,
        None
    );
    assert_eq!(
        second.iter().find(|p| p.pid == 5200).unwrap().util_pct,
        Some(0.0)
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn arc_a580_and_a310_resolve_by_name() {
    let scratch = scratch_copy("i915-arc-ids", "intel-i915-kernel6.8");
    let id = scratch.join("sys/class/drm/card1/device/device");
    for (pci, name) in [("0x56a2", "Intel Arc A580"), ("0x56a6", "Intel Arc A310")] {
        std::fs::write(&id, format!("{pci}\n")).unwrap();
        let mut b = IntelBackend::with_root(&scratch).unwrap();
        let dev = b.devices().remove(0);
        assert_eq!(b.static_info(&dev).unwrap().name, name);
    }

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- degradation cases ----

#[test]
fn igpu_minimal_everything_optional_is_none() {
    let mut b = IntelBackend::with_root(fixture("intel-igpu-minimal")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:00:02.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "Intel GPU [8086:46a6]"); // not in the table → PCI-id fallback
                                                    // No lmem_total_bytes: the iGPU shares system RAM, which must NOT appear as VRAM.
    assert_eq!(info.mem_total_bytes, None);
    assert_eq!(info.power_limit_mw, None); // no hwmon at all — the normal iGPU case
    assert_eq!(info.max_sm_clock_mhz, None);

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, None);
    assert_eq!(s.mem_used_bytes, None);
    assert_eq!(s.temp_c, None);
    assert_eq!(s.power_mw, None);
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.sm_clock_mhz, None);
    assert_eq!(s.mem_clock_mhz, None);

    // No proc tree at all: an empty process list, never an error.
    assert!(b.refresh_processes(&devs[0]).unwrap().is_empty());
}

#[test]
fn no_intel_devices_is_a_clean_unavailable() {
    // The fixtures directory itself has no sys/class/drm — init must report Unavailable,
    // which the registry treats as a normal skipped backend.
    match IntelBackend::with_root(fixture("")) {
        Err(e) => assert!(matches!(e, BackendError::Unavailable(_))),
        Ok(_) => panic!("expected Unavailable for a tree without sys/class/drm"),
    }
}

/// A fresh scratch copy of a committed fixture tree, for tests that mutate counters.
/// `tag` must be unique per test (tests run in parallel within one process).
fn scratch_copy(tag: &str, tree: &str) -> PathBuf {
    let scratch =
        std::env::temp_dir().join(format!("gpuviewer-intel-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture(tree), &scratch);
    scratch
}

/// Advance a cumulative counter file by `by`.
fn bump_u64(path: &Path, by: u64) {
    let cur: u64 = std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::write(path, format!("{}\n", cur + by)).unwrap();
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}
