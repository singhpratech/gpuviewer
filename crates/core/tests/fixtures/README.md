# Test fixture trees

**These trees are SYNTHETIC** — hand-written for deterministic tests, not captured from
real hardware. The values are realistic but invented. Per the testing strategy in
CLAUDE.md, they should be replaced/augmented with trees captured from real machines, with
new captures taken per kernel/driver release.

- `amd-rx7900xtx-kernel6.8/` — models a Radeon RX 7900 XTX (Navi 31, `1002:744c` rev
  `c8`) on the in-tree amdgpu driver around kernel 6.8: full sysfs device metrics, hwmon
  (edge/junction/mem temps in milli-°C, power in µW, fan RPM + max), `pp_dpm_*` DPM
  tables with a '*'-marked current level, a libdrm `amdgpu.ids` name database, and a
  `/proc` tree with DRM fdinfo clients using the pre-6.13-deprecation `drm-memory-*`
  keys plus `drm-pdev` and per-engine busy-ns. pid 4521 (python3) holds two fds on the
  device (dedupe-by-max coverage) and a busy compute engine; pid 2210 (gnome-shell) is
  graphics-only; pid 980 (Xorg) sits on a *different* GPU's pdev and must be filtered
  out of this device's process list.
- `amd-igpu-minimal/` — an APU-style device exposing only the enumeration files
  (vendor, device, uevent): no hwmon, no `pp_dpm_*`, no VRAM/busy files, no `/proc`
  tree. Every optional metric must come back `None` without an error.
- `amd-strixpoint-kernel6.10/` — models a Strix Point class APU (`1002:150e` — PCI id is
  a placeholder, confirm at capture time) on kernel ~6.10: APU hwmon (edge `temp1` only,
  `power1_average` plus a `power1_input` decoy that must stay un-preferred, no fan, no
  `power1_cap`), `pp_dpm_*` tables, the 512 MiB UMA carve-out as `mem_info_vram_*` plus
  `mem_info_gtt_*` files (unread today — pre-staged for the GTT work), and a
  `gpu_metrics` **v3_0** blob (sizeof 264). The blob carries residency ACCUMULATORS at
  the compiled offsets 228–255 (prochot=37, spl=9, sppt=4, thm_gfx=12, thm_soc=3), a
  `current_gfx_maxfreq` decoy (2900) @224, and **the killer decoy: the 2-byte struct pad
  @226–227 is 0xFF** (the SMU memsets the table before writing) — exactly what an
  off-by-−2 decoder misreads as a 0xFFFF prochot. v3_0 has no instantaneous throttle
  word, so the per-sample decode must be `None` despite the nonzero accumulators.
- `amd-vangogh-steamdeck-kernel6.8/` — models a Van Gogh / Steam Deck APU (`1002:163f`)
  on kernel 6.6+ program-6 firmware: hwmon with Van Gogh's unique `power1_label`=slowPPT
  and a `power2_*`=fastPPT decoy channel (power1 must stay read), edge temp, no fan; the
  1 GiB `mem_info_vram_*` carve-out plus 8 GiB `mem_info_gtt_*` (where Deck games really
  allocate — unread today, never silently summed into VRAM); and a `gpu_metrics` **v2_4**
  blob declaring `structure_size` = **168**, the kernel's real `sizeof` (164 data bytes +
  4 u64-alignment tail-pad bytes, 0xFF'd kernel-true) — the regression fixture for the
  164→168 size-gate fix that silently dropped every current-firmware Deck sample.
  indep@120 = SPPT_APU; legacy@108 = 0x40 decoy; fan_pwm/padding @112–119 = 0xFF. `/proc`
  has a game pid 1337 (gfx busy-ns, small VRAM, large GTT) and a media-only pid 2001
  (`drm-engine-dec/enc` + GTT only — every unconsumed key must degrade to honest `None`).
- `amd-cyanskillfish-bc250-kernel6.8/` — models a Cyan Skillfish / BC-250 class board
  (`1002:13fe`, PCI id approximate) whose firmware emits `gpu_metrics` **v2_2** (128)
  but **never writes `indep_throttle_status`**: bytes @120–127 are the SMU's 0xFF memset
  sentinel (as are the unwritten fan_pwm/padding @112–119), while legacy
  `throttle_status`@108 = 0 is a genuine observed quiet. The decode must fall through
  the sentinel to `Some(all-false)` — before the sentinel guard this hardware narrated
  thermal+power+hw_slowdown+other on every sample, permanently. Minimal sysfs otherwise
  (no `pp_dpm_*`, no fan, no cap).
- `amd-vega20-radeonvii-kernel6.8/` — models a Radeon VII (Vega 20, `1002:66af` rev `c1`):
  full SOC15 dGPU hwmon in the real channel order (temp1 edge 71 °C, temp2 junction /
  temp3 mem decoys, `temp1_crit` and `power1_cap_max` and `pwm1` decoys, fan RPM + max),
  an `amdgpu.ids` with a same-device different-revision decoy line, and a `gpu_metrics`
  **v1_0** blob (sizeof 80): legacy `throttle_status`@68 = 0x40 (ASIC-specific — decodes
  only as coarse `other`), `current_fan_speed`@72 and pcie width/speed @74–75 as adjacent
  decoys, and the 4-byte header pad @4–7 plus tail pad @76–79 filled with 0xFF (the swsmu
  memset convention; whether Vega 20's powerplay path memsets the same way is unverified —
  those bytes are never read either way).
- `amd-mi300-kernel6.12/` — models an Instinct MI300X (`1002:74a1`, PCI id approximate)
  on a kernel 6.7–6.12: hwmon with **no temp1/edge channel** (temp2 = junction 68 °C,
  temp3 = mem 74 °C — the hotter decoy that must never be reported as device
  temperature), `power1_input` only, no fan, no cap; and a `gpu_metrics` **v1_5** blob
  (sizeof 360) laid out on the SMU's 0xFF memset: legacy `throttle_status`@104 = 0x2,
  `system_clock_counter`@96 and `gfxclk_lock_status`@108 = 0xFF as adjacent decoys, idle
  VCN clocks plus `padding`/tail pad @354–359 left 0xFF. No v1_7 blob is committed: the
  6.13+ residency revisions' total sizeof is not pinned by the in-repo docs, so their
  tests build blobs in-test at the literal prefix offsets instead of committing a
  fabricated size. MI300 XCP partition topology vs PCI dedupe is **[HW-VERIFY]**.
- `amd-polaris-rx580-kernel6.8/` — models an RX 580 (Polaris 20, `1002:67df` rev `e7`):
  pre-SMU-metrics hardware with **no `gpu_metrics` node at all** (GC < 9.1.0 hides it),
  so throttle must be `None`; full hwmon (edge temp1, `power1_average`, `power1_cap`, fan
  RPM + max, `temp1_crit`/`pwm1` decoys), an 8-level `pp_dpm_sclk` in `Mhz` casing, and
  one game pid (dota2) in the 6.8 fdinfo dialect.
- `amd-phoenix-apu-kernel6.8/` — models a Phoenix APU (Radeon 780M, `1002:15bf` rev `c4`):
  edge temp1 only, `power1_input` µW only, no fan/cap, a 512 MiB carve-out plus 15 GiB
  `mem_info_gtt_*` (unread here), and a `gpu_metrics` **v2_1** blob (sizeof 120) with
  legacy `throttle_status`@108 = 0 between a nonzero `current_l3clk` decoy @104 and the
  0xFF'd `fan_pwm`/padding @112–119. `/proc` holds a GTT-dominant game pid 5150
  (192 MiB VRAM, 2.5 GiB GTT, `amd-requested-*`/`amd-evicted-*` decoys) and a VAAPI
  playback pid 6060 (mpv: `drm-engine-dec` plus a little gfx) that drives the
  media-engine-counts-toward-util test.
- `amd-fdinfo-kernel6.13/` — a minimal device (`1002:7550`) whose `/proc` carries the
  standardized `drm_print_memory_stats` keys (`drm-total/shared/active/resident/
  purgeable-{cpu,gtt,vram}`, bytes by default, scaled to KiB/MiB only when exactly
  divisible). pid 3001 is forward-looking (legacy `drm-memory-*` alias removed, resident
  "512 MiB", `drm-total-vram`/`amd-requested-vram` decoys); pid 3002 is 6.13-true (both
  key families, "98308 KiB", agreeing); pid 3003 has a bare-bytes resident 12345678 whose
  legacy alias floor-divides to 12056 KiB (the kernel-true decoy for a legacy-first
  parser); pid 3004 is an idle client with no engine lines (util must stay `None`, not 0).
- `amd-fdinfo-kernel5.15/` — a minimal device whose one client speaks the 5.14–5.18
  fdinfo dialect (`pdev:`, `vram mem: … kB`, `gfx0: 37.5%`). That dialect is not parsed
  (the parse gate is 5.19+), so the process probe reports Unavailable — an unknown list —
  rather than an empty list that would claim nothing is attached.
- `intel-i915-kernel6.8/` — models an Arc A770 dGPU (DG2, `8086:56a0`) on the in-tree
  i915 driver around kernel 6.8: card-level `gt_act_freq_mhz`/`gt_cur_freq_mhz`/
  `gt_RP0_freq_mhz` and `lmem_total_bytes`, hwmon with `power1_max` (µW) and the
  cumulative `energy1_input` (µJ) counter but NO temp/fan (those gates are i915 6.12+),
  and a `/proc` tree with i915-dialect fdinfo: `drm-engine-*` cumulative busy-NS plus
  6.8-era `drm-total-local0`/`drm-resident-local0` memory regions. pid 3100 (ffplay)
  holds two fds (dedupe-by-max coverage) with a busy video engine; pid 5200 (blender)
  is render-only (honestly Unknown kind); pid 6300 (python3) has a busy compute
  engine; pid 980 (Xorg) sits on a different pdev and must be filtered out. The per-GT
  `gt/gt0/throttle_reason_*` flags ship quiescent (status=0, every reason 0); throttle
  tests drive scratch copies to assert the bit→model mapping and the status gate.
- `intel-xe-kernel6.11/` — models an Arc B580 dGPU (BMG, `8086:e20b`) on the xe driver
  at the kernel 6.11 fdinfo/sysfs ABI (the `drm-cycles-*` keys landed in 6.11):
  `device/tile0/gt0/freq0/{act_freq,cur_freq,rp0_freq}`, hwmon power/energy only (xe
  temp gate is 6.15+, fans 6.16+), and xe-dialect fdinfo: `drm-cycles-*` with
  `drm-total-cycles-*` GT-tick bases (NOT time units) and `drm-total-vram0` memory
  regions. There is deliberately no VRAM-total sysfs file — xe has none. The
  `tile0/gt0/freq0/throttle/{status,reason_*}` flags ship quiescent (status=0); the
  xe-specific filename spelling (`reason_pl1`, not i915's `throttle_reason_pl1`) is what
  the throttle test exercises — the metric intel_gpu_top still does not surface on xe.
- `intel-igpu-minimal/` — an iGPU (ADL-P, `8086:46a6`, i915) exposing only the
  enumeration files: no hwmon (the normal iGPU case), no `lmem_*` (shares system RAM,
  which must never be reported as VRAM), no freq files, no `/proc` tree. Every
  optional metric must come back `None` without an error.
- `intel-i915-kernel6.12-arc/` — the Arc A770 i915 tree advanced to the kernel 6.12 ABI,
  where i915 dGPU hwmon temps/fans landed: `temp1_input` = 61000 milli-°C is the REAL
  package sensor (i915's documented channel), `temp2_input` = 53000 is a DECOY a
  wrong-dialect read would pick up, and `fan1_input` = 2100 RPM has no `fan1_max`
  reference so `fan_pct` must stay `None`. Also carries a per-GT `gt/gt0/rps_act_freq_mhz`
  decoy (1500, differing from the card-level `gt_act_freq_mhz` 1850 — proves which freq
  file is read) and an rc6 residency decoy pair (`gt/gt0/rc6_residency_ms` 480000 vs the
  legacy `power/rc6_residency_ms` 480250) pre-staged for future GT-awake% work.
- `intel-xe-kernel6.15-bmg/` — the Arc B580 xe tree advanced to the kernel 6.15 ABI,
  where xe hwmon temps landed. xe's channel layout differs from i915's: the package
  sensor is `temp2_input` = 58000 milli-°C (REAL), `temp3_input` = 64000 is the VRAM
  sensor (DECOY as device temp — a different physical claim), and `temp1_input` = 47000
  is a pure DECOY: real xe hwmon exposes NO temp1, but the historical bug read i915's
  temp1 channel on both dialects, so the file exists precisely to catch that wrong code
  path. Channel-choice decoys `power2_max` (220 W pkg) / `energy2_input` assert the card
  channel (`power1_max`/`energy1_input`) stays preferred; `fan1_input` = 1450 RPM with no
  max asserts `fan_pct` stays `None`; `freq0/rpa_freq` (2200) is a freq-file decoy; and
  `tile0/gt0/gtidle/idle_residency_ms` is pre-staged for future GT-awake% work.
