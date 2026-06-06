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
- `intel-i915-kernel6.8/` — models an Arc A770 dGPU (DG2, `8086:56a0`) on the in-tree
  i915 driver around kernel 6.8: card-level `gt_act_freq_mhz`/`gt_cur_freq_mhz`/
  `gt_RP0_freq_mhz` and `lmem_total_bytes`, hwmon with `power1_max` (µW) and the
  cumulative `energy1_input` (µJ) counter but NO temp/fan (those gates are i915 6.12+),
  and a `/proc` tree with i915-dialect fdinfo: `drm-engine-*` cumulative busy-NS plus
  6.8-era `drm-total-local0`/`drm-resident-local0` memory regions. pid 3100 (ffplay)
  holds two fds (dedupe-by-max coverage) with a busy video engine; pid 5200 (blender)
  is render-only (honestly Unknown kind); pid 6300 (python3) has a busy compute
  engine; pid 980 (Xorg) sits on a different pdev and must be filtered out.
- `intel-xe-kernel6.11/` — models an Arc B580 dGPU (BMG, `8086:e20b`) on the xe driver
  at the kernel 6.11 fdinfo/sysfs ABI (the `drm-cycles-*` keys landed in 6.11):
  `device/tile0/gt0/freq0/{act_freq,cur_freq,rp0_freq}`, hwmon power/energy only (xe
  temp gate is 6.15+, fans 6.16+), and xe-dialect fdinfo: `drm-cycles-*` with
  `drm-total-cycles-*` GT-tick bases (NOT time units) and `drm-total-vram0` memory
  regions. There is deliberately no VRAM-total sysfs file — xe has none.
- `intel-igpu-minimal/` — an iGPU (ADL-P, `8086:46a6`, i915) exposing only the
  enumeration files: no hwmon (the normal iGPU case), no `lmem_*` (shares system RAM,
  which must never be reported as VRAM), no freq files, no `/proc` tree. Every
  optional metric must come back `None` without an error.
