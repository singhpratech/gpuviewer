#![cfg(target_os = "linux")]
//! AMD backend fixture tests — run against the SYNTHETIC trees under `tests/fixtures/`
//! (see the README there). Unit conversions are asserted against exact raw values
//! because unit bugs (milli-C vs C, µW vs mW, KiB vs bytes) are the classic AMD sysfs
//! parsing failure mode.

use std::path::{Path, PathBuf};

use gpuviewer_core::amd::{decode_gpu_metrics_throttle, AmdBackend};
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
    // Media-only client (drm-engine-dec/enc, GTT memory only): those keys are not yet
    // consumed, so every per-process metric is honest absence — never a crash, never a
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

/// Every `(format, content)` revision the decoder supports, v1_1 through v2_4. v3_0 is
/// recognised-but-None and pinned separately by the strixpoint blob test.
///
/// v1_x (dGPU): legacy@68; indep landed in v1_3 @112. v2_x (APU): v2_0's reordered
/// prefix puts legacy@112; v2_1+ share legacy@108 and grow indep@120 from v2_2.
#[rustfmt::skip]
const KERNEL_LAYOUTS: &[KernelLayout] = &[
    KernelLayout { name: "v1_1", format: 1, content: 1, size: 96,  legacy: Some(68),  indep: None },
    KernelLayout { name: "v1_2", format: 1, content: 2, size: 104, legacy: Some(68),  indep: None },
    KernelLayout { name: "v1_3", format: 1, content: 3, size: 120, legacy: Some(68),  indep: Some(112) },
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
