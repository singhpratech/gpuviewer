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
        3,
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
