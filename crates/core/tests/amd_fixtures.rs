#![cfg(target_os = "linux")]
//! AMD backend fixture tests — run against the SYNTHETIC trees under `tests/fixtures/`
//! (see the README there). Unit conversions are asserted against exact raw values
//! because unit bugs (milli-C vs C, µW vs mW, KiB vs bytes) are the classic AMD sysfs
//! parsing failure mode.

use std::path::{Path, PathBuf};

use gpuviewer_core::amd::{
    decode_gpu_metrics_throttle, decode_gpu_metrics_throttle_delta, AmdBackend,
};
use gpuviewer_core::{BackendError, DeviceId, GpuBackend, ProcessKind, ThrottleReasons, Vendor};

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
    // The temperature comes from the edge-labeled sensor: nothing to qualify.
    assert_eq!(info.source_caveat, None);
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
    // ...and the choice of an unconfirmed channel is surfaced, never silent.
    assert_eq!(
        b.static_info(&dev).unwrap().source_caveat.as_deref(),
        Some("temp is unlabeled hwmon temp1 (amdgpu's edge channel by channel order)")
    );

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

// ---- amd-strixpoint-kernel6.10: gpu_metrics v3_0 (Strix Point class APU) ---------------

#[test]
fn strixpoint_v3_0_tree_decodes_metrics_with_throttle_unobservable() {
    let mut b = AmdBackend::with_root(fixture("amd-strixpoint-kernel6.10")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:c4:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.vendor, Vendor::Amd);
    assert_eq!(info.name, "AMD GPU [1002:150e]"); // no amdgpu.ids in this tree → PCI fallback
                                                  // On an APU, sysfs "VRAM" is the BIOS UMA carve-out (512 MiB here) — reported as-is;
                                                  // GTT, where real allocations live, is separate future work and must never be
                                                  // silently summed into this number.
    assert_eq!(info.mem_total_bytes, Some(536_870_912));
    assert_eq!(info.power_limit_mw, None); // no power1_cap on this APU
    assert_eq!(info.max_sm_clock_mhz, Some(2900));

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, Some(23.0));
    assert_eq!(s.mem_used_bytes, Some(201_326_592));
    assert_eq!(s.temp_c, Some(58.0)); // edge-labeled temp1, milli-C → C
                                      // power1_average must stay preferred over the power1_input decoy (15012000 µW).
    assert_eq!(s.power_mw, Some(14_237));
    assert_eq!(s.fan_pct, None); // APU: no fan files at all
    assert_eq!(s.sm_clock_mhz, Some(1900));
    assert_eq!(s.mem_clock_mhz, Some(1334));
    // The headline: v3_0 carries no instantaneous throttle word — only residency
    // ACCUMULATORS (nonzero in this blob: a prochot fired at SOME point in the
    // firmware's window). A single sample cannot honestly turn an accumulator into a
    // live state, so the per-sample decode is None (unobservable) — never the
    // permanent hw_slowdown the old −2/raw-nonzero decoder narrated.
    assert_eq!(
        s.throttle, None,
        "v3_0 per-sample throttle must be unobservable"
    );
}

#[test]
fn strixpoint_blob_pins_v3_0_kernel_offsets_and_pad_sentinel() {
    // Pins the committed blob's bytes at the compiled-offsetof literals
    // (docs/research/07-non-nvidia-coverage.md §2.2: residency u32s at 228..256 behind
    // a 2-byte pad after `current_gfx_maxfreq` u16 @224), so a future blob
    // re-generation cannot silently drift off the kernel layout.
    let blob = std::fs::read(fixture(
        "amd-strixpoint-kernel6.10/sys/class/drm/card0/device/gpu_metrics",
    ))
    .unwrap();
    let u32_at = |off: usize| u32::from_le_bytes(blob[off..off + 4].try_into().unwrap());
    assert_eq!(blob.len(), 264); // compiled sizeof(gpu_metrics_v3_0)
    assert_eq!(u16::from_le_bytes([blob[0], blob[1]]), 264);
    assert_eq!((blob[2], blob[3]), (3, 0));
    assert_eq!(u16::from_le_bytes([blob[224], blob[225]]), 2900); // current_gfx_maxfreq decoy
                                                                  // The killer decoy: the struct pad is 0xFF on real silicon (SMU memsets the table
                                                                  // before writing) — exactly what a −2 decoder misreads as a 0xFFFF "prochot".
    assert_eq!(blob[226..228], [0xFF, 0xFF]);
    assert_eq!(u32_at(228), 37); // throttle_residency_prochot
    assert_eq!(u32_at(232), 9); // throttle_residency_spl
    assert_eq!(u32_at(236), 0); // throttle_residency_fppt
    assert_eq!(u32_at(240), 4); // throttle_residency_sppt
    assert_eq!(u32_at(244), 0); // throttle_residency_thm_core
    assert_eq!(u32_at(248), 12); // throttle_residency_thm_gfx
    assert_eq!(u32_at(252), 3); // throttle_residency_thm_soc
                                // Residency counters are monotonic accumulators, not status bits: the pure
                                // single-blob decode must be None regardless of their values — and the 0xFF pad
                                // must never leak into any reason.
    assert_eq!(decode_gpu_metrics_throttle(&blob), None);
}

// ---- amd-vangogh-steamdeck-kernel6.8: gpu_metrics v2_4 at the REAL sizeof 168 ----------

#[test]
fn vangogh_steamdeck_v2_4_decodes_at_sizeof_168_regression() {
    let mut b = AmdBackend::with_root(fixture("amd-vangogh-steamdeck-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:04:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "AMD GPU [1002:163f]");
    // 1 GiB carve-out (the Deck default) — honestly understates APU memory until GTT
    // lands as its own labeled metric; never the 8 GiB mem_info_gtt_total.
    assert_eq!(info.mem_total_bytes, Some(1_073_741_824));
    assert_eq!(info.power_limit_mw, None);
    assert_eq!(info.max_sm_clock_mhz, Some(1600));

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, Some(88.0));
    assert_eq!(s.temp_c, Some(67.0));
    // power1 is Van Gogh's slowPPT channel; the fastPPT power2_average decoy
    // (11456000 µW) must not be read.
    assert_eq!(s.power_mw, Some(8_123));
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.sm_clock_mhz, Some(700));
    assert_eq!(s.mem_clock_mhz, None); // no pp_dpm_mclk in this tree
                                       // THE regression assertion: the blob declares structure_size = 168 = the kernel's
                                       // sizeof (164 data bytes + 4 u64-alignment tail-pad bytes, smu_cmn.h). The old 164
                                       // gate silently rejected every real v2_4 blob — i.e. every current-firmware Steam
                                       // Deck sample — as None.
    let t = s
        .throttle
        .expect("a real 168-byte v2_4 blob must decode (164→168 gate fix)");
    assert!(t.power_cap, "SPPT_APU (bit 7) decodes to power_cap");
    assert!(
        !t.thermal && !t.hw_slowdown && !t.sync_boost && !t.other,
        "legacy 0x40 decoy, 0xFF'd fan_pwm/padding, and 0xFF tail pad must all be ignored"
    );
}

#[test]
fn vangogh_blob_pins_v2_4_size_and_word_offsets() {
    let blob = std::fs::read(fixture(
        "amd-vangogh-steamdeck-kernel6.8/sys/class/drm/card0/device/gpu_metrics",
    ))
    .unwrap();
    assert_eq!(blob.len(), 168);
    assert_eq!(
        u16::from_le_bytes([blob[0], blob[1]]),
        168,
        "structure_size = sizeof (168), never the 164 data length"
    );
    assert_eq!((blob[2], blob[3]), (2, 4));
    assert_eq!(
        u32::from_le_bytes(blob[108..112].try_into().unwrap()),
        0x40,
        "legacy throttle_status decoy"
    );
    // fan_pwm + padding[3]: Van Gogh has no fan — firmware never writes these, so on
    // real silicon they hold the 0xFF memset (also the off-by-8 decoy before indep).
    assert_eq!(blob[112..120], [0xFF; 8]);
    assert_eq!(
        u64::from_le_bytes(blob[120..128].try_into().unwrap()),
        1 << 7, // SMU_THROTTLER_SPPT_APU_BIT
    );
    assert_eq!(blob[164..168], [0xFF; 4], "kernel-true 0xFF tail pad");
    // Re-declaring the old wrong gate's 164 over a truncated copy must be rejected:
    // that is not the struct the kernel ships.
    let mut short = blob.clone();
    short.truncate(164);
    short[0..2].copy_from_slice(&164u16.to_le_bytes());
    assert_eq!(decode_gpu_metrics_throttle(&short), None);
}

#[test]
fn vangogh_fdinfo_game_and_media_pids() {
    let mut b = AmdBackend::with_root(fixture("amd-vangogh-steamdeck-kernel6.8")).unwrap();
    let dev = b.devices().remove(0);

    let procs = b.refresh_processes(&dev).unwrap();
    assert_eq!(procs.len(), 2);

    let game = &procs[0];
    assert_eq!((game.pid, game.name.as_str()), (1337, "portal2"));
    assert_eq!(game.kind, ProcessKind::Graphics);
    // VRAM key only: 262144 KiB → bytes. The 3 GiB drm-memory-gtt (where Deck games
    // really allocate) must NOT be summed in — GTT is a label-it-separately story, and
    // silently adding it here would fabricate a VRAM number.
    assert_eq!(game.mem_bytes, Some(268_435_456));
    assert_eq!(game.util_pct, None); // first sighting: no engine-ns baseline

    let media = &procs[1];
    assert_eq!((media.pid, media.name.as_str()), (2001, "ffmpeg"));
    // Media-only client (drm-engine-dec/enc, GTT memory only). The media engines are
    // consumed now, but a first sighting has no baseline → util None; GTT is not VRAM
    // (and is not read yet), so mem is honest absence — never a crash, never a
    // fabricated 0%.
    assert_eq!(media.util_pct, None);
    assert_eq!(media.mem_bytes, None);
    assert_eq!(media.kind, ProcessKind::Graphics); // no compute signal observed
}

// ---- amd-cyanskillfish-bc250-kernel6.8: v2_2 with the 0xFF indep sentinel --------------

#[test]
fn cyanskillfish_v2_2_ff_sentinel_is_quiet_never_active_throttle() {
    let mut b = AmdBackend::with_root(fixture("amd-cyanskillfish-bc250-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:05:00.0".into())]);

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    // Cyan Skillfish firmware never writes indep_throttle_status, so it holds the
    // SMU's 0xFF memset: that word is absent (not 64 simultaneous throttlers), and
    // decoding falls through to the legacy word — 0 here, a genuine OBSERVED quiet.
    // Before the sentinel guard this device narrated thermal+power+hw_slowdown+other
    // on every sample, permanently.
    let t = s
        .throttle
        .expect("legacy word stays observable behind the indep sentinel");
    assert!(
        !t.any(),
        "the 0xFF sentinel must never surface as an active throttle"
    );
    assert_eq!(t, ThrottleReasons::default());

    // The rest of the tree decodes normally around the sentinel.
    assert_eq!(s.util_pct, Some(41.0));
    assert_eq!(s.temp_c, Some(54.0));
    assert_eq!(s.power_mw, Some(95_000));
    assert_eq!(s.sm_clock_mhz, None); // no pp_dpm_* tables in this tree
    assert_eq!(s.mem_clock_mhz, None);
    assert_eq!(s.fan_pct, None);
}

#[test]
fn cyanskillfish_blob_pins_v2_2_sentinel_bytes() {
    let blob = std::fs::read(fixture(
        "amd-cyanskillfish-bc250-kernel6.8/sys/class/drm/card0/device/gpu_metrics",
    ))
    .unwrap();
    assert_eq!(blob.len(), 128); // compiled sizeof(gpu_metrics_v2_2)
    assert_eq!(u16::from_le_bytes([blob[0], blob[1]]), 128);
    assert_eq!((blob[2], blob[3]), (2, 2));
    assert_eq!(
        u32::from_le_bytes(blob[108..112].try_into().unwrap()),
        0,
        "legacy throttle_status: an observed quiet"
    );
    assert_eq!(
        blob[120..128],
        [0xFF; 8],
        "indep_throttle_status never written — the memset sentinel"
    );
    assert_eq!(
        decode_gpu_metrics_throttle(&blob),
        Some(ThrottleReasons::default())
    );
}

// ---- gpu_metrics blob-builder coverage for EVERY supported revision --------------------
//
// One supported revision's kernel layout, as OFFSET LITERALS transcribed from the
// compiled-offsetof cross-check in docs/research/07-non-nvidia-coverage.md §2.2
// (kgd_pp_interface.h, natural alignment, little-endian, structure_size = sizeof per
// smu_cmn.h). These are deliberately NOT derived from the decoder's own layout table —
// sharing a constant with the code under test would re-create the self-confirming-builder
// trap the audit flagged: the in-module tests passed for months while the v3_0 offsets
// were wrong, because builder and decoder shared the same mistake.
//
// Units note for the future temps/power decoder (today's decoder reads only the throttle
// words, which are bitfields and carry no unit): v1_x temperatures are °C and power W;
// v2_x and v3_0 temperatures are centi-°C and power mW (kgd_pp_interface.h comments;
// renoir_ppt.c and smu_v14_0_0_ppt.c divide by 100). The committed v2_x fixture blobs
// above already carry centi-°C-magnitude temperature words at their kernel offsets so
// that decoder lands on pre-staged unit coverage.
struct KernelLayout {
    name: &'static str,
    format: u8,
    content: u8,
    /// Compiled `sizeof` — includes tail padding (v2_4's famous +4).
    size: usize,
    /// `offsetof(.., throttle_status)` — the legacy ASIC-specific u32.
    legacy: Option<usize>,
    /// `offsetof(.., indep_throttle_status)` — the ASIC-independent u64.
    indep: Option<usize>,
}

/// Every `(format, content)` revision the decoder supports per sample, v1_0 through v2_4.
/// v3_0 is recognised-but-None and pinned separately by the strixpoint blob test; the
/// v1_7/v1_8 residency revisions are delta-decoded and covered by their own tests below.
///
/// v1_x (dGPU): v1_0's own prefix and v1_1–v1_3 share legacy@68 (v1_0 sizeof 80); indep
/// landed in v1_3 @112; MI300's v1_5 moves legacy to @104 behind the VCN/JPEG activity
/// arrays (sizeof 360). v2_x (APU): v2_0's reordered prefix puts legacy@112; v2_1+ share
/// legacy@108 and grow indep@120 from v2_2.
#[rustfmt::skip]
const KERNEL_LAYOUTS: &[KernelLayout] = &[
    KernelLayout { name: "v1_0", format: 1, content: 0, size: 80,  legacy: Some(68),  indep: None },
    KernelLayout { name: "v1_1", format: 1, content: 1, size: 96,  legacy: Some(68),  indep: None },
    KernelLayout { name: "v1_2", format: 1, content: 2, size: 104, legacy: Some(68),  indep: None },
    KernelLayout { name: "v1_3", format: 1, content: 3, size: 120, legacy: Some(68),  indep: Some(112) },
    KernelLayout { name: "v1_5", format: 1, content: 5, size: 360, legacy: Some(104), indep: None },
    KernelLayout { name: "v2_0", format: 2, content: 0, size: 120, legacy: Some(112), indep: None },
    KernelLayout { name: "v2_1", format: 2, content: 1, size: 120, legacy: Some(108), indep: None },
    KernelLayout { name: "v2_2", format: 2, content: 2, size: 128, legacy: Some(108), indep: Some(120) },
    KernelLayout { name: "v2_3", format: 2, content: 3, size: 152, legacy: Some(108), indep: Some(120) },
    KernelLayout { name: "v2_4", format: 2, content: 4, size: 168, legacy: Some(108), indep: Some(120) },
];

/// Build a blob for `l`: every non-header byte set to `fill`, then the given words
/// written at the layout's literal offsets. A `fill` of 0xAB poisons every adjacent
/// offset — a read that is off by even one byte mixes poison into the word and decodes
/// to the wrong categories, so the exact-equality assertions below pin the offsets.
fn build_at_literals(
    l: &KernelLayout,
    fill: u8,
    legacy: Option<u32>,
    indep: Option<u64>,
) -> Vec<u8> {
    let mut b = vec![fill; l.size];
    b[0..2].copy_from_slice(&(l.size as u16).to_le_bytes());
    b[2] = l.format;
    b[3] = l.content;
    if let (Some(off), Some(v)) = (l.legacy, legacy) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    if let (Some(off), Some(v)) = (l.indep, indep) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    b
}

#[test]
fn every_version_indep_word_is_read_at_the_kernel_offset() {
    // TEMP_HOTSPOT (bit 36) is thermal-only. The blob is 0xAB-poisoned everywhere else
    // (including the legacy word): a decoder reading the indep word anywhere but the
    // literal offset picks up poison bytes, whose bits land in power/other and fail the
    // exact-equality check.
    for l in KERNEL_LAYOUTS.iter().filter(|l| l.indep.is_some()) {
        let b = build_at_literals(l, 0xAB, None, Some(1 << 36));
        let t = decode_gpu_metrics_throttle(&b)
            .unwrap_or_else(|| panic!("{}: a well-formed blob must decode", l.name));
        assert_eq!(
            t,
            ThrottleReasons {
                thermal: true,
                ..Default::default()
            },
            "{}: indep must be read at exactly @{} and preferred over the legacy word",
            l.name,
            l.indep.unwrap()
        );
    }
}

#[test]
fn every_version_legacy_word_is_read_at_the_kernel_offset() {
    // The indep word (where the version has one) is set to the 0xFF sentinel so decoding
    // reaches the legacy word, exactly like sentinel-shipping hardware.
    for l in KERNEL_LAYOUTS.iter().filter(|l| l.legacy.is_some()) {
        // Side 1 — zero blob, nonzero legacy at the literal offset: coarse `other`
        // alone (the ASIC-specific bits are never mapped to a guessed cause). A decoder
        // reading any always-zero offset instead sees quiet and fails.
        let b = build_at_literals(l, 0x00, Some(0x2), Some(u64::MAX));
        let t = decode_gpu_metrics_throttle(&b)
            .unwrap_or_else(|| panic!("{}: a well-formed blob must decode", l.name));
        assert_eq!(
            t,
            ThrottleReasons {
                other: true,
                ..Default::default()
            },
            "{}: nonzero legacy @{} is a coarse `other`, never a specific cause",
            l.name,
            l.legacy.unwrap()
        );
        // Side 2 — poison blob, ZERO legacy at the literal offset: an observed quiet.
        // Any read off by even one byte mixes 0xAB in and decodes nonzero; for indep
        // versions a shifted indep read also stops matching the 0xFF sentinel and
        // decodes garbage bits — both directions fail loudly.
        let b = build_at_literals(l, 0xAB, Some(0), Some(u64::MAX));
        assert_eq!(
            decode_gpu_metrics_throttle(&b),
            Some(ThrottleReasons::default()),
            "{}: zero legacy @{} is an observed quiet",
            l.name,
            l.legacy.unwrap()
        );
    }
}

#[test]
fn every_version_sentinel_words_decode_to_absence_never_throttle() {
    for l in KERNEL_LAYOUTS {
        match (l.indep, l.legacy) {
            (Some(_), Some(_)) => {
                // indep sentinel falls through to a quiet legacy word → observed quiet.
                let b = build_at_literals(l, 0x00, Some(0), Some(u64::MAX));
                assert_eq!(
                    decode_gpu_metrics_throttle(&b),
                    Some(ThrottleReasons::default()),
                    "{}: indep 0xFF sentinel must fall through to the legacy word",
                    l.name
                );
                // Both words sentinel'd → this firmware exposes no throttle at all.
                let b = build_at_literals(l, 0x00, Some(u32::MAX), Some(u64::MAX));
                assert_eq!(
                    decode_gpu_metrics_throttle(&b),
                    None,
                    "{}: all-sentinel words are unobservable, never Some(anything)",
                    l.name
                );
            }
            (None, Some(_)) => {
                // Legacy-only version with a sentinel'd legacy word → unobservable.
                let b = build_at_literals(l, 0x00, Some(u32::MAX), None);
                assert_eq!(
                    decode_gpu_metrics_throttle(&b),
                    None,
                    "{}: a 0xFF'd legacy word means unsupported, not throttling hard",
                    l.name
                );
            }
            _ => unreachable!("every supported layout carries a legacy word"),
        }
    }
}

#[test]
fn every_version_size_gate_matches_the_kernel_sizeof_exactly() {
    for l in KERNEL_LAYOUTS {
        // At exactly the compiled sizeof, a quiet blob decodes — this pins the
        // decoder's size gate to the literal (and so cross-checks its layout table
        // against the kernel headers without sharing any constant with it).
        let ok = build_at_literals(l, 0x00, Some(0), Some(0));
        assert_eq!(
            decode_gpu_metrics_throttle(&ok),
            Some(ThrottleReasons::default()),
            "{}: a blob at the kernel sizeof {} must decode",
            l.name,
            l.size
        );
        // The generalized v2_4 bug-class: a −4 claim (data bytes instead of sizeof)
        // over a matching shorter buffer is NOT the struct the kernel ships → None.
        let mut short = build_at_literals(l, 0x00, Some(0), Some(0));
        short.truncate(l.size - 4);
        short[0..2].copy_from_slice(&((l.size - 4) as u16).to_le_bytes());
        assert_eq!(
            decode_gpu_metrics_throttle(&short),
            None,
            "{}: structure_size {} (sizeof − 4) must be rejected",
            l.name,
            l.size - 4
        );
        // A grown struct reusing this revision tag is not ours either: trusting our
        // offsets inside an unknown larger layout would be a guess.
        let mut long = build_at_literals(l, 0x00, Some(0), Some(0));
        long.extend_from_slice(&[0u8; 8]);
        long[0..2].copy_from_slice(&((l.size + 8) as u16).to_le_bytes());
        assert_eq!(
            decode_gpu_metrics_throttle(&long),
            None,
            "{}: structure_size {} (sizeof + 8) must be rejected",
            l.name,
            l.size + 8
        );
    }
}

// ---- amd-vega20-radeonvii-kernel6.8: gpu_metrics v1_0 (Radeon VII / MI50 / MI60) --------

#[test]
fn vega20_v1_0_tree_decodes_dgpu_metrics_and_coarse_throttle() {
    let mut b = AmdBackend::with_root(fixture("amd-vega20-radeonvii-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:0c:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    // Keyed by device AND revision: the C0 decoy line names a different board.
    assert_eq!(info.name, "AMD Radeon VII");
    assert_eq!(info.mem_total_bytes, Some(17_163_091_968));
    // power1_cap (250 W), never the power1_cap_max ceiling decoy (300 W).
    assert_eq!(info.power_limit_mw, Some(250_000));
    assert_eq!(info.max_sm_clock_mhz, Some(1801));
    // temp1_crit is shutdown-adjacent, not the slowdown knee — still not claimed.
    assert_eq!(info.temp_slowdown_c, None);
    assert_eq!(info.source_caveat, None); // edge sensor present

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, Some(99.0));
    // Real SOC15 channel order: temp1 = edge (71 °C); junction 94 / mem 78 are decoys.
    assert_eq!(s.temp_c, Some(71.0));
    assert_eq!(s.power_mw, Some(248_000));
    assert_eq!(s.fan_pct, Some(50.0)); // 1925 of 3850 RPM — never the pwm1 duty decoy
    assert_eq!(s.sm_clock_mhz, Some(1750));
    assert_eq!(s.mem_clock_mhz, Some(1001));
    // v1_0 was previously rejected (None) — Radeon VII's throttle story was discarded.
    // Its only throttle word is the ASIC-specific legacy throttle_status (nonzero here):
    // a coarse `other`, never a guessed specific cause.
    assert_eq!(
        s.throttle,
        Some(ThrottleReasons {
            other: true,
            ..Default::default()
        })
    );
}

#[test]
fn vega20_blob_pins_v1_0_kernel_offsets() {
    let blob = std::fs::read(fixture(
        "amd-vega20-radeonvii-kernel6.8/sys/class/drm/card0/device/gpu_metrics",
    ))
    .unwrap();
    assert_eq!(blob.len(), 80); // compiled sizeof(gpu_metrics_v1_0), 07 §2.2
    assert_eq!(u16::from_le_bytes([blob[0], blob[1]]), 80);
    assert_eq!((blob[2], blob[3]), (1, 0));
    assert_eq!(u32::from_le_bytes(blob[68..72].try_into().unwrap()), 0x40);
    // fan/pcie decoys @72-75 adjacent to the word, then the struct tail pad.
    assert_eq!(u16::from_le_bytes([blob[72], blob[73]]), 1925);
    assert_eq!((blob[74], blob[75]), (16, 80));
    assert_eq!(blob[76..80], [0xFF; 4]);
    // Zero the word at the literal offset: an observed quiet. A decoder reading @72
    // (fan decoy) or @64 (current clocks) would still see nonzero and say `other`.
    let mut quiet = blob.clone();
    quiet[68..72].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_gpu_metrics_throttle(&quiet),
        Some(ThrottleReasons::default())
    );
}

// ---- amd-mi300-kernel6.12: gpu_metrics v1_5, junction-only hwmon ----------------------

#[test]
fn mi300_junction_only_hwmon_reports_junction_with_a_caveat() {
    let mut b = AmdBackend::with_root(fixture("amd-mi300-kernel6.12")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:15:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "AMD GPU [1002:74a1]");
    assert_eq!(info.mem_total_bytes, Some(206_158_430_208));
    assert_eq!(info.power_limit_mw, None); // no power1_cap in this tree
                                           // The chosen sensor is surfaced: junction, not edge.
    assert_eq!(
        info.source_caveat.as_deref(),
        Some(
            "temp is the junction (hotspot) sensor: this device has no edge sensor, and junction reads hotter than edge"
        )
    );

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    // MI300 has no temp1/edge channel. Before the fallback chain this was None despite
    // a real junction reading; now it is the junction (68 °C) — never the hotter HBM
    // `mem` sensor (74 °C decoy).
    assert_eq!(s.temp_c, Some(68.0));
    // power1_input only (no power1_average): µW → mW.
    assert_eq!(s.power_mw, Some(612_000));
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.util_pct, Some(100.0));
    assert_eq!(s.sm_clock_mhz, Some(1420));
    // v1_5 (previously rejected): legacy throttle_status @104 nonzero → coarse `other`.
    assert_eq!(
        s.throttle,
        Some(ThrottleReasons {
            other: true,
            ..Default::default()
        })
    );
}

#[test]
fn mi300_blob_pins_v1_5_kernel_offsets() {
    let blob = std::fs::read(fixture(
        "amd-mi300-kernel6.12/sys/class/drm/card0/device/gpu_metrics",
    ))
    .unwrap();
    assert_eq!(blob.len(), 360); // compiled sizeof(gpu_metrics_v1_5), 07 §2.2
    assert_eq!(u16::from_le_bytes([blob[0], blob[1]]), 360);
    assert_eq!((blob[2], blob[3]), (1, 5));
    let u32_at = |off: usize| u32::from_le_bytes(blob[off..off + 4].try_into().unwrap());
    assert_eq!(u32_at(104), 0x2); // throttle_status
    assert_ne!(u32_at(100), 0); // system_clock_counter high half — decoy before
    assert_eq!(u32_at(108), 0xFF); // gfxclk_lock_status — decoy after
    assert_eq!(blob[354..360], [0xFF; 6]); // padding + tail pad, SMU memset
    let mut quiet = blob.clone();
    quiet[104..108].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_gpu_metrics_throttle(&quiet),
        Some(ThrottleReasons::default()),
        "a zero word at exactly @104 is an observed quiet"
    );
    // The v1_4 layout (legacy@40) must not be confused with v1_5: @40 here is mid-jpeg
    // activity, zero — a decoder applying v1_4's offset would report quiet, not `other`.
    assert_eq!(u32_at(40), 0);
}

#[test]
fn temp_fallback_never_reports_a_mem_sensor() {
    // MI300 with the junction channel gone leaves only `mem` (HBM): that is a different
    // physical claim than device temperature, so temp is None — not 74 °C.
    let scratch =
        std::env::temp_dir().join(format!("gpuviewer-amd-memtemp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("amd-mi300-kernel6.12"), &scratch);
    let hwmon = scratch.join("sys/class/drm/card0/device/hwmon/hwmon5");
    std::fs::remove_file(hwmon.join("temp2_label")).unwrap();
    std::fs::remove_file(hwmon.join("temp2_input")).unwrap();

    let mut b = AmdBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert_eq!(b.refresh_dynamic(&dev).unwrap().temp_c, None);
    assert_eq!(b.static_info(&dev).unwrap().source_caveat, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- gpu_metrics v1_7 / v1_8: residency accumulators, delta-decoded --------------------

/// Build a v1_7/v1_8 blob with the residency block at the LITERAL offsets from 07 §2.2
/// (accumulation_counter @40, prochot/ppt/socket_thm/vr_thm/hbm_thm residency-acc
/// @44/48/52/56/60), everything else 0xAB-poisoned. `size` is deliberately NOT presented
/// as a kernel sizeof: the in-repo docs do not pin these revisions' total size (their
/// tails are version-elastic), so the decoder prefix-gates and so do these tests.
fn build_residency(content: u8, size: usize, acc: u32, res: [u32; 5]) -> Vec<u8> {
    let mut b = vec![0xAB; size];
    b[0..2].copy_from_slice(&(size as u16).to_le_bytes());
    b[2] = 1;
    b[3] = content;
    b[40..44].copy_from_slice(&acc.to_le_bytes());
    for (i, v) in res.iter().enumerate() {
        let off = 44 + 4 * i;
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    b
}

#[test]
fn residency_revisions_are_unobservable_per_sample_and_decode_as_deltas() {
    for content in [7, 8] {
        let prev = build_residency(content, 1024, 10_000, [3, 4, 5, 6, 7]);
        // A single blob — however nonzero its accumulators — says nothing about NOW.
        assert_eq!(decode_gpu_metrics_throttle(&prev), None, "v1_{content}");
        // Nothing advanced over a real window: an observed quiet.
        let same = build_residency(content, 1024, 10_100, [3, 4, 5, 6, 7]);
        assert_eq!(
            decode_gpu_metrics_throttle_delta(&prev, &same),
            Some(ThrottleReasons::default()),
            "v1_{content}"
        );
        // Each counter lands in its own column. A read off by one word would shift
        // every category (prochot→power_cap, etc.) and fail these exact matches.
        let cases = [
            ([4, 4, 5, 6, 7], (false, false, true)), // prochot → hw_slowdown
            ([3, 5, 5, 6, 7], (false, true, false)), // ppt → power_cap
            ([3, 4, 6, 6, 7], (true, false, false)), // socket thermal
            ([3, 4, 5, 7, 7], (true, false, false)), // VR thermal
            ([3, 4, 5, 6, 8], (true, false, false)), // HBM thermal
        ];
        for (res, (thermal, power_cap, hw_slowdown)) in cases {
            let cur = build_residency(content, 1024, 10_100, res);
            assert_eq!(
                decode_gpu_metrics_throttle_delta(&prev, &cur),
                Some(ThrottleReasons {
                    thermal,
                    power_cap,
                    hw_slowdown,
                    ..Default::default()
                }),
                "v1_{content} {res:?}"
            );
        }
    }
}

#[test]
fn residency_delta_refuses_what_it_cannot_honestly_window() {
    let prev = build_residency(7, 1024, 10_000, [3, 4, 5, 6, 7]);
    let cur = build_residency(7, 1024, 10_100, [4, 4, 5, 6, 7]);
    // Prefix gate: a structure_size that cannot hold the residency block is rejected.
    // The bytes are physically present; the self-declared size says they are not ours.
    let mut short = build_residency(7, 1024, 10_100, [4, 4, 5, 6, 7]);
    short[0..2].copy_from_slice(&63u16.to_le_bytes());
    assert_eq!(decode_gpu_metrics_throttle_delta(&prev, &short), None);
    // And a structure_size claiming more bytes than the read returned is truncation.
    let mut truncated = build_residency(7, 1024, 10_100, [4, 4, 5, 6, 7]);
    truncated.truncate(512);
    assert_eq!(decode_gpu_metrics_throttle_delta(&prev, &truncated), None);
    // The 0xFF memset sentinel in any counter: unobservable, never "saturated".
    let ff = build_residency(7, 1024, 10_100, [4, u32::MAX, 5, 6, 7]);
    assert_eq!(decode_gpu_metrics_throttle_delta(&prev, &ff), None);
    let ff_acc = build_residency(7, 1024, u32::MAX, [4, 4, 5, 6, 7]);
    assert_eq!(decode_gpu_metrics_throttle_delta(&prev, &ff_acc), None);
    // No new firmware window, or a reset (counter backwards).
    assert_eq!(decode_gpu_metrics_throttle_delta(&prev, &prev), None);
    assert_eq!(decode_gpu_metrics_throttle_delta(&cur, &prev), None);
    // Only v1_7/v1_8 carry this block: the same bytes tagged v1_6 (header-only, never
    // emitted) or v1_9 (attribute-vector watch item) are not delta-decoded.
    for content in [6, 9] {
        let p = build_residency(content, 1024, 10_000, [3, 4, 5, 6, 7]);
        let c = build_residency(content, 1024, 10_100, [4, 4, 5, 6, 7]);
        assert_eq!(
            decode_gpu_metrics_throttle_delta(&p, &c),
            None,
            "v1_{content}"
        );
        assert_eq!(decode_gpu_metrics_throttle(&c), None, "v1_{content}");
    }
    // Status-word revisions are not residency revisions.
    let l = &KERNEL_LAYOUTS[0];
    let v1_0 = build_at_literals(l, 0x00, Some(0x2), None);
    assert_eq!(decode_gpu_metrics_throttle_delta(&v1_0, &v1_0), None);
}

#[test]
fn never_emitted_v1_4_and_v1_6_stay_unobservable() {
    // Header-defined but never observed from a tagged kernel (07 §2.2): even at the
    // audit's v1_4 numbers (legacy@40, sizeof 288) with a nonzero word, no decode.
    let mut b = vec![0u8; 288];
    b[0..2].copy_from_slice(&288u16.to_le_bytes());
    b[2] = 1;
    b[3] = 4;
    b[40..44].copy_from_slice(&0x2u32.to_le_bytes());
    assert_eq!(decode_gpu_metrics_throttle(&b), None);
    b[3] = 6;
    assert_eq!(decode_gpu_metrics_throttle(&b), None);
}

#[test]
fn mi300_backend_delta_decodes_v1_7_across_ticks() {
    // A 6.13+ MI300 emits v1_7: the backend keeps a per-device residency watermark.
    let scratch = std::env::temp_dir().join(format!("gpuviewer-amd-v1_7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("amd-mi300-kernel6.12"), &scratch);
    let node = scratch.join("sys/class/drm/card0/device/gpu_metrics");
    let mut b = AmdBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);

    std::fs::write(&node, build_residency(7, 1024, 10_000, [37, 9, 0, 0, 12])).unwrap();
    // First read: a baseline only. The nonzero historic accumulators are NOT a claim.
    assert_eq!(b.refresh_dynamic(&dev).unwrap().throttle, None);

    std::fs::write(&node, build_residency(7, 1024, 10_050, [37, 9, 0, 0, 12])).unwrap();
    assert_eq!(
        b.refresh_dynamic(&dev).unwrap().throttle,
        Some(ThrottleReasons::default()),
        "no counter moved over the window: observed quiet"
    );

    std::fs::write(&node, build_residency(7, 1024, 10_100, [40, 9, 0, 0, 12])).unwrap();
    assert_eq!(
        b.refresh_dynamic(&dev).unwrap().throttle,
        Some(ThrottleReasons {
            hw_slowdown: true,
            ..Default::default()
        }),
        "PROCHOT residency advanced during this window"
    );

    // Firmware did not refresh the table: no new window → no claim (baseline kept).
    assert_eq!(b.refresh_dynamic(&dev).unwrap().throttle, None);

    // The node vanishing drops the watermark: the next read is a fresh baseline, never
    // a delta across the unobserved gap.
    std::fs::remove_file(&node).unwrap();
    assert_eq!(b.refresh_dynamic(&dev).unwrap().throttle, None);
    std::fs::write(&node, build_residency(7, 1024, 20_000, [90, 9, 0, 0, 12])).unwrap();
    assert_eq!(b.refresh_dynamic(&dev).unwrap().throttle, None);

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- amd-polaris-rx580-kernel6.8: pre-SMU, hwmon only ---------------------------------

#[test]
fn polaris_pre_smu_tree_degrades_to_none_cleanly() {
    let mut b = AmdBackend::with_root(fixture("amd-polaris-rx580-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:01:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "Radeon RX 580 Series");
    assert_eq!(info.mem_total_bytes, Some(8_589_934_592));
    assert_eq!(info.power_limit_mw, Some(145_000));
    // 8-level table with "Mhz" casing: the last level is the max.
    assert_eq!(info.max_sm_clock_mhz, Some(1340));
    assert_eq!(info.source_caveat, None);

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, Some(54.0));
    assert_eq!(s.temp_c, Some(66.0));
    assert_eq!(s.power_mw, Some(98_000));
    assert_eq!(s.fan_pct, Some(50.0));
    assert_eq!(s.sm_clock_mhz, Some(1257));
    assert_eq!(s.mem_clock_mhz, Some(2000));
    // GC < 9.1.0 hides the gpu_metrics node entirely: throttle is unobservable (None),
    // never an asserted all-false "not throttling".
    assert_eq!(s.throttle, None);

    let procs = b.refresh_processes(&devs[0]).unwrap();
    assert_eq!(procs.len(), 1);
    assert_eq!((procs[0].pid, procs[0].name.as_str()), (4242, "dota2"));
    assert_eq!(procs[0].mem_bytes, Some(1_073_741_824)); // 1048576 KiB, never the GTT
    assert_eq!(procs[0].kind, ProcessKind::Graphics);
}

// ---- amd-phoenix-apu-kernel6.8: realistic mainstream APU, v2_1 -------------------------

#[test]
fn phoenix_apu_v2_1_tree() {
    let mut b = AmdBackend::with_root(fixture("amd-phoenix-apu-kernel6.8")).unwrap();
    let devs = b.devices();
    assert_eq!(devs, vec![DeviceId("0000:c3:00.0".into())]);

    let info = b.static_info(&devs[0]).unwrap();
    assert_eq!(info.name, "AMD Radeon 780M Graphics");
    // The 512 MiB UMA carve-out as-is — never the 15 GiB GTT total.
    assert_eq!(info.mem_total_bytes, Some(536_870_912));
    assert_eq!(info.power_limit_mw, None);
    assert_eq!(info.max_sm_clock_mhz, Some(2700));
    assert_eq!(info.source_caveat, None);

    let s = b.refresh_dynamic(&devs[0]).unwrap();
    assert_eq!(s.util_pct, Some(31.0));
    assert_eq!(s.mem_used_bytes, Some(402_653_184));
    assert_eq!(s.temp_c, Some(52.0));
    assert_eq!(s.power_mw, Some(18_734)); // power1_input µW → mW, no power1_average
    assert_eq!(s.fan_pct, None);
    assert_eq!(s.sm_clock_mhz, Some(1700));
    assert_eq!(s.mem_clock_mhz, Some(2800));
    // v2_1 legacy-only, word zero at @108 between a nonzero l3clk decoy @104 and the
    // 0xFF'd fan_pwm/padding @112: an observed quiet. Off by −4 reads nonzero
    // (`other`); off by +4 reads the sentinel (None).
    assert_eq!(s.throttle, Some(ThrottleReasons::default()));

    let procs = b.refresh_processes(&devs[0]).unwrap();
    assert_eq!(procs.len(), 2);
    let game = &procs[0];
    assert_eq!((game.pid, game.name.as_str()), (5150, "factorio"));
    // GTT-dominant client: VRAM is 192 MiB of carve-out; the 2.5 GiB GTT and the
    // amd-requested-* keys are different claims and are never substituted or summed.
    assert_eq!(game.mem_bytes, Some(201_326_592));
    assert_eq!(game.kind, ProcessKind::Graphics);
    let mpv = &procs[1];
    assert_eq!((mpv.pid, mpv.name.as_str()), (6060, "mpv"));
    assert_eq!(mpv.mem_bytes, Some(33_554_432));
    assert_eq!(mpv.util_pct, None); // first sighting
}

#[test]
fn phoenix_media_engine_counts_toward_process_util() {
    let scratch = std::env::temp_dir().join(format!("gpuviewer-amd-media-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&fixture("amd-phoenix-apu-kernel6.8"), &scratch);
    let mut b = AmdBackend::with_root(&scratch).unwrap();
    let dev = b.devices().remove(0);
    assert!(b
        .refresh_processes(&dev)
        .unwrap()
        .iter()
        .all(|p| p.util_pct.is_none()));

    // Advance ONLY mpv's decode engine, far past any plausible wall delta. Its gfx
    // counter is unchanged, so a gfx-only util would read 0% — the busiest engine (dec)
    // is what must surface (clamped to 100).
    std::thread::sleep(std::time::Duration::from_millis(60));
    let fd = scratch.join("proc/6060/fdinfo/14");
    let bumped = std::fs::read_to_string(&fd).unwrap().replace(
        "drm-engine-dec:\t2500000000 ns",
        "drm-engine-dec:\t902500000000 ns",
    );
    std::fs::write(&fd, bumped).unwrap();

    let procs = b.refresh_processes(&dev).unwrap();
    let mpv = procs.iter().find(|p| p.pid == 6060).unwrap();
    assert_eq!(mpv.util_pct, Some(100.0));
    assert_eq!(mpv.kind, ProcessKind::Graphics); // media is graphics-side work
                                                 // The game's counters did not move: 0 busy-ns over the window = an observed 0%.
    let game = procs.iter().find(|p| p.pid == 5150).unwrap();
    assert_eq!(game.util_pct, Some(0.0));

    let _ = std::fs::remove_dir_all(&scratch);
}

// ---- amd-fdinfo-kernel6.13 / kernel5.15: fdinfo dialects -------------------------------

#[test]
fn fdinfo_standardized_resident_vram_scales_bytes_kib_mib() {
    let mut b = AmdBackend::with_root(fixture("amd-fdinfo-kernel6.13")).unwrap();
    let dev = b.devices().remove(0);
    let procs = b.refresh_processes(&dev).unwrap();
    let mem = |pid: u32| procs.iter().find(|p| p.pid == pid).unwrap().mem_bytes;
    // Standardized keys only (legacy alias removed): "512 MiB" — never drm-total-vram
    // (768 MiB), drm-resident-gtt (64 MiB) or amd-requested-vram (768 MiB).
    assert_eq!(mem(3001), Some(536_870_912));
    // "98308 KiB" (not MiB-aligned, so the kernel kept KiB) — agreeing legacy alias.
    assert_eq!(mem(3002), Some(100_667_392));
    // Bare bytes: resident is preferred over the legacy alias, which floor-divides to
    // 12056 KiB = 12345344 bytes — a kernel-true decoy for a legacy-first parser.
    assert_eq!(mem(3003), Some(12_345_678));
    assert_eq!(mem(3004), Some(4_194_304));
}

#[test]
fn fdinfo_client_without_engine_lines_is_none_not_zero() {
    let mut b = AmdBackend::with_root(fixture("amd-fdinfo-kernel6.13")).unwrap();
    let dev = b.devices().remove(0);
    b.refresh_processes(&dev).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let procs = b.refresh_processes(&dev).unwrap();
    // Second sighting: a client with no engine lines has no engine observation to
    // window — None, never a fabricated 0%. Clients WITH static engine counters are an
    // observed 0%.
    let util = |pid: u32| procs.iter().find(|p| p.pid == pid).unwrap().util_pct;
    assert_eq!(util(3004), None);
    assert_eq!(util(3001), Some(0.0));
}

#[test]
fn fdinfo_pre_5_19_dialect_is_an_unknown_list_never_an_empty_one() {
    // 5.14–5.18 printed "pdev:"/"vram mem:"/"gfx0: 37.5%" — not parsed (the parse gate
    // is 5.19+). A client of this device IS attached, so an empty list would be a
    // positive "nothing attached" claim; the probe reports Unavailable instead, which
    // the collector carries as an unknown (null) process list.
    let mut b = AmdBackend::with_root(fixture("amd-fdinfo-kernel5.15")).unwrap();
    let dev = b.devices().remove(0);
    match b.refresh_processes(&dev) {
        Err(BackendError::Unavailable(why)) => assert!(why.contains("5.19"), "{why}"),
        other => panic!("expected Unavailable, got {other:?}"),
    }
    // The device itself still reports normally around the unreadable process data.
    assert!(b.refresh_dynamic(&dev).is_ok());
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
