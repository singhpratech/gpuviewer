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
- `intel-i915-kernel6.8/` — models an Arc A770 dGPU (DG2, `8086:56a0`) on the in-tree
  i915 driver around kernel 6.8: card-level `gt_act_freq_mhz`/`gt_cur_freq_mhz`/
  `gt_RP0_freq_mhz` and `lmem_total_bytes`, hwmon with `power1_max` (µW) and the
  cumulative `energy1_input` (µJ) counter but NO temp/fan (those gates are i915 6.12+),
  and a `/proc` tree with i915-dialect fdinfo: `drm-engine-*` cumulative busy-NS plus
  6.8-era `drm-total-local0`/`drm-resident-local0` memory regions. pid 3100 (ffplay)
  holds two fds (dedupe-by-max coverage) with a busy video engine; pid 5200 (blender)
  is render-only (honestly Unknown kind); pid 6300 (python3) has a busy compute
  engine; pid 980 (Xorg) sits on a different pdev and must be filtered out; pid 7400
  (kwin_wayland) is the **pre-gate shape** — `drm-pdev` plus `local0`/`system0` memory
  regions but NO `drm-engine-*` keys at all (a synthetic key-subset case, not a specific
  kernel's capture: it pins the path where the engine counters are simply absent) —
  and must be listed with its VRAM, `Unknown` kind, and `util_pct` `None` on every
  sighting, never a 0% invented from missing keys. The per-GT
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
- `intel-xe-dg2-forced/` — an Arc A770 (DG2, `8086:56a0`) force-probed onto the **xe**
  driver (DRIVER=xe — a real user configuration; DG2 stays i915-default), kernel ~6.12
  ABI. Its hwmon is **pkg-only**: `power2_max` = 225 W and `energy2_input`, with NO
  index-1 (card) power/energy files — a card-only read reports `None` while a real
  value sits in sysfs (research 07 §3.2.2). `power2_rated_max` = 190 W is a DECOY (the
  default TDP, not the enforced PL1). Power limit AND derived draw both come from the
  pkg channel, and `source_caveat` labels the reading package-scope. No temp files (xe
  temps gate 6.15+). xe freq layout with an `rp0_freq`/`max_freq` decoy pair; one
  python3 pid with xe cycles, `vram0` memory, and `drm-engine-capacity-ccs: 4`.
  Which channels a real DG2-on-xe box exposes is [HW-VERIFY] — this tree models the
  documented channel scheme, not a capture.
- `intel-i915-igpu-mtl/` — a Meteor Lake iGPU (`8086:7d55`, DRIVER=i915, `0000:00:02.0`):
  no hwmon, no `lmem_total_bytes`, card-level freq files (act 1650 / RP0 2250 / user-cap
  decoy 2000). **Two GTs**: `gt/gt0` throttle flags quiescent, while `gt/gt1` — MTL's
  MEDIA GT — has `throttle_reason_status` = 1 with thermal + pl1 set, plus
  `rps_act`/`rps_RP0` freq decoys: only gt0 may be consulted, or the render device
  narrates a throttle that is not happening. fdinfo regions are `system0` and
  `stolen-system0` only (system RAM — `mem_bytes` must stay `None`, never VRAM), with
  i915 busy-ns engines: pid 2900 (gnome-shell) render-only → `Unknown`; pid 4700
  (ffmpeg) busy video → Graphics, carrying `drm-engine-capacity-video: 2` (a different
  class's capacity, which must not touch render utilization).
- `intel-xe-igpu-lnl/` — a Lunar Lake iGPU (`8086:64a0`, DRIVER=xe, `0000:00:02.0`): NO
  hwmon dir at all (xe registers hwmon for discrete parts only), `tile0/gt0/freq0/*`
  (act 1450 / RP0 2050 / user-cap decoy 1900) plus a `tile0/gt1` MEDIA-GT decoy set
  (act 900, RP0 1300, and an active `throttle/status` = 1 with `reason_thermal` = 1).
  fdinfo carries `system`/`gtt`/`stolen` regions and **no `vram0`** (`mem_bytes`
  `None`), and xe `drm-cycles-*`/`drm-total-cycles-*` counters so per-process
  utilization (Δbusy / Δtotal cycles) works on an iGPU: pid 3300 (gnome-shell) rcs-only
  → `Unknown`; pid 6100 (mpv) busy vcs → Graphics.
