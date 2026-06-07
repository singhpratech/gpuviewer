#![cfg(target_os = "linux")]
//! AMD backend fixture tests — run against the SYNTHETIC trees under `tests/fixtures/`
//! (see the README there). Unit conversions are asserted against exact raw values
//! because unit bugs (milli-C vs C, µW vs mW, KiB vs bytes) are the classic AMD sysfs
//! parsing failure mode.

use std::path::{Path, PathBuf};

use gpuviewer_core::amd::AmdBackend;
use gpuviewer_core::{BackendError, DeviceId, GpuBackend, ProcessKind, Vendor};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn rx7900xtx_enumeration_and_static_info() {
    let mut b = AmdBackend::with_root(fixture("amd-rx7900xtx-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:03:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.vendor, Vendor::Amd);
    assert_eq!(info.backend, "amd");
    // Marketing name resolved through the fixture's amdgpu.ids by device id + revision.
    assert_eq!(info.name, "AMD Radeon RX 7900 XTX");
    // 24 GiB, bytes as-is.
    assert_eq!(info.mem_total_bytes, Some(25_769_803_776));
    // power1_cap is MICROwatts: 339000000 µW must become 339000 mW (339 W).
    assert_eq!(info.power_limit_mw, Some(339_000));
    // The last pp_dpm_sclk level is the hardware max.
    assert_eq!(info.max_sm_clock_mhz, Some(2890));
    assert_eq!(info.driver_version, None);
    // The fixture root has no proc/self/status, so privilege is unknown → the backend
    // must claim incompleteness rather than overpromise.
    assert_eq!(
        info.process_hint.as_deref(),
        Some("showing your processes only — others need root or CAP_SYS_PTRACE (fdinfo)")
    );
}

#[test]
fn rx7900xtx_dynamic_sample_unit_conversions() {
    let mut b = AmdBackend::with_root(fixture("amd-rx7900xtx-kernel6.8")).unwrap();
    let dev = b.devices().remove(0);

    let s = b.refresh_dynamic(&dev).unwrap();
    assert_eq!(s.util_pct, Some(67.0));
    assert_eq!(s.mem_used_bytes, Some(8_489_271_296)); // bytes as-is

    // 53000 milli-C → 53.0 C, and it MUST come from the "edge"-labeled sensor: in this
    // fixture edge is temp2 (junction is temp1 at 61000), so a broken label search that
    // falls through to temp1_input reads 61.0 and fails here.
    assert_eq!(s.temp_c, Some(53.0));
    assert_eq!(s.power_mw, Some(284_000)); // 284000000 µW → 284000 mW
    assert_eq!(s.fan_pct, Some(50.0)); // 1650 RPM of 3300 max
    assert_eq!(s.sm_clock_mhz, Some(1138)); // the '*'-marked pp_dpm_sclk level
    assert_eq!(s.mem_clock_mhz, Some(1249)); // the '*'-marked pp_dpm_mclk level
    assert_eq!(s.encoder_pct, None);
    assert_eq!(s.decoder_pct, None);
    // The fixture's gpu_metrics node is a real v1_3 blob with SMU_THROTTLER_PPT0 set in
    // indep_throttle_status: a genuine power-cap throttle decodes here. The blob also
    // carries an off-by-8 thermal decoy and a nonzero legacy throttle_status, so a decoder
    // that read the wrong offset (or fell back to the legacy ASIC-specific word) would set
    // thermal/other instead of power_cap and fail these assertions.
    let t = s
        .throttle
        .expect("gpu_metrics present → throttle observable");
    assert!(t.power_cap, "PPT0 bit decodes to power_cap");
    assert!(!t.thermal, "no thermal bit set — decoy must be ignored");
    assert!(!t.other, "indep is preferred over the legacy word");
}

#[test]
fn rx7900xtx_fdinfo_processes() {
    let mut b = AmdBackend::with_root(fixture("amd-rx7900xtx-kernel6.8")).unwrap();
    let dev = b.devices().remove(0);

    let procs = b.refresh_processes(&dev).unwrap();
    assert_eq!(
        procs.len(),
        2,
        "the pid on the other GPU's pdev must be filtered out"
    );

    let shell = &procs[0];
    assert_eq!((shell.pid, shell.name.as_str()), (2210, "gnome-shell"));
    assert_eq!(shell.kind, ProcessKind::Graphics); // drm-engine-compute is 0 ns
    assert_eq!(shell.mem_bytes, Some(536_870_912)); // 524288 KiB → bytes

    let py = &procs[1];
    assert_eq!((py.pid, py.name.as_str()), (4521, "python3"));
    // drm-engine-compute > 0 ns is decisive.
    assert_eq!(py.kind, ProcessKind::Compute);
    // Max across the pid's two fds: 6815744 KiB → 6979321856 bytes — never the 4096 KiB
    // fd, and never a sum of the two.
    assert_eq!(py.mem_bytes, Some(6_979_321_856));
    // First sighting has no engine-ns baseline.
    assert_eq!(py.util_pct, None);
    assert_eq!(shell.util_pct, None);
}

#[test]
fn fdinfo_engine_delta_yields_util_on_second_sighting() {
    // Watermark plumbing needs a counter that moves, so this test works on a scratch
    // copy of the fixture rather than the committed tree.
    let scratch =
        std::env::temp_dir().join(format!("gpuviewer-amd-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("amd-rx7900xtx-kernel6.8"), &scratch);

    let mut b = AmdBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    let first = b.refresh_processes(&dev).unwrap();
    assert!(first.iter().all(|p| p.util_pct.is_none()));

    // Advance python's cumulative gfx-ns far past 100% of any plausible wall delta:
    // utilization must clamp, proving delta/wall (not the raw counter) is reported.
    std::thread::sleep(std::time::Duration::from_millis(60));
    let fd = scratch.join("proc/4521/fdinfo/12");
    let bumped = std::fs::read_to_string(&fd).unwrap().replace(
        "drm-engine-gfx:\t1500000000 ns",
        "drm-engine-gfx:\t901500000000 ns",
    );
    std::fs::write(&fd, bumped).unwrap();

    let second = b.refresh_processes(&dev).unwrap();
    let py = second.iter().find(|p| p.pid == 4521).unwrap();
    assert_eq!(py.util_pct, Some(100.0));
    // gnome-shell's counter did not move: 0 busy-ns over the same wall = 0%.
    let shell = second.iter().find(|p| p.pid == 2210).unwrap();
    assert_eq!(shell.util_pct, Some(0.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn temp_without_labels_falls_back_to_temp1_input() {
    let scratch = std::env::temp_dir().join(format!("gpuviewer-amd-temps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("amd-rx7900xtx-kernel6.8"), &scratch);
    let hwmon = scratch.join("sys/class/drm/card1/device/hwmon/hwmon3");
    for n in 1..=3 {
        std::fs::remove_file(hwmon.join(format!("temp{n}_label"))).unwrap();
    }

    let mut b = AmdBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    // No labels to search: temp1_input (junction here, 61000 milli-C) is the fallback.
    assert_eq!(b.refresh_dynamic(&dev).unwrap().temp_c, Some(61.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn power_falls_back_to_power1_input_when_average_is_absent() {
    // RDNA3 / kernel 6.7+ can expose only the instantaneous power1_input and no
    // power1_average — the fallback probe must still convert µW → mW.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-amd-power-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("amd-rx7900xtx-kernel6.8"), &scratch);
    let hwmon = scratch.join("sys/class/drm/card1/device/hwmon/hwmon3");
    std::fs::remove_file(hwmon.join("power1_average")).unwrap();
    std::fs::write(hwmon.join("power1_input"), "297500000\n").unwrap();

    let mut b = AmdBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().power_mw, Some(297_500));

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn igpu_minimal_everything_optional_is_none() {
    let mut b = AmdBackend::with_root(fixture("amd-igpu-minimal")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:c1:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "AMD GPU [1002:15bf]"); // no amdgpu.ids here → PCI-id fallback
    assert_eq!(info.mem_total_bytes, None);
    assert_eq!(info.power_limit_mw, None);
    assert_eq!(info.max_sm_clock_mhz, None);

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, None);
    assert_eq!(s.mem_used_bytes, None);
    assert_eq!(s.temp_c, None);
    assert_eq!(s.power_mw, None);
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.sm_clock_mhz, None);
    assert_eq!(s.mem_clock_mhz, None);
    // No gpu_metrics node on this APU fixture: the throttle source does not exist →
    // None (unobservable, §5.4), never an asserted all-false "not throttling".
    assert_eq!(s.throttle, None);

    // No proc tree at all: an empty process list, never an error.
    assert!(b.refresh_processes(&devs[0]).unwrap().is_empty());
}

#[test]
fn no_amd_devices_is_a_clean_unavailable() {
    // The fixtures directory itself has no sys/class/drm — init must report Unavailable,
    // which the registry treats as a normal skipped backend.
    match AmdBackend::with_root(fixture("")) {
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
