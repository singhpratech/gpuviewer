//! Shared Linux per-process metadata helpers used across every Linux backend.
//!
//! GPU drivers tell us *which* PIDs are touching the GPU and how much VRAM they hold; the
//! rest of a process's identity lives in `/proc`. These helpers turn that into two columns
//! the story feed leans on:
//! - **CPU %** ([`CpuTracker`]): a GPU at 3% with python pinning a CPU core is the classic
//!   dataloader-bound fingerprint — the GPU is starved, not idle.
//! - **container identity** ([`container_of`]): "the run is `docker:1a2b3c4d5e6f`" is what a
//!   cluster operator actually needs to find the offending pod.
//!
//! Per the domain rules in CLAUDE.md every value is `Option` and absence is normal: a PID
//! can exit between the GPU listing it and us reading `/proc`, `/proc/<pid>` may be a foreign
//! user's process we cannot read, and corrupt/racy reads must yield `None`, never a panic or
//! a fabricated number. The parsers are split out as pure functions so they unit-test
//! without a `/proc` at all.

use std::collections::HashMap;
use std::time::Instant;

/// `USER_HZ` — the kernel reports `/proc/<pid>/stat` CPU times in clock ticks, and this is
/// the divisor to seconds. It is configurable in theory (`CONFIG_HZ`) but `sysconf(_SC_CLK_TCK)`
/// returns 100 on every Linux arch/distro gpuviewer supports, and there is no syscall-free
/// way to read it; hard-coding 100 keeps the core crate dependency-free and is correct on the
/// targets we ship. (If a wrong-HZ kernel ever surfaces, the clamp below still bounds the lie.)
const USER_HZ: f64 = 100.0;

/// CPU % is reported relative to one core (100.0 = one full core saturated), so a 64-core box
/// could legitimately read 6400. Anything past that is a clock-skew / counter-reset artifact,
/// not a real measurement — clamp to it rather than emit a nonsense spike into the chart.
const CPU_PCT_MAX: f32 = 6400.0;

/// Parse a container runtime out of `/proc/<pid>/cgroup` content. Pure so it tests against
/// captured cgroup strings — no `/proc` required.
///
/// cgroup v2 puts the whole hierarchy on one `0::<path>` line; v1 has many `id:ctrl:path`
/// lines. We scan every line's path for a runtime's signature scope/dir name, because the
/// signature can sit at any depth (systemd slices nest it under `system.slice`, kubelet under
/// `kubepods.slice/kubepods-besteffort.slice/...`). The first runtime we recognize wins.
///
/// Returns a short, stable label (`docker:<12 hex>`, `k8s:<12>`, `podman:<12>`, `lxc:<name>`)
/// or `None` for a host process. We deliberately do NOT invent an id when none is parseable
/// (bare `kubepods` with no pod id → `k8s:?`): a truncated id presented as exact would be a
/// quiet lie, and matching the wrong pod is worse than admitting we only know "some pod".
pub fn parse_cgroup(content: &str) -> Option<String> {
    for line in content.lines() {
        // v1 lines are `hierarchy-id:controllers:path`; v2 is `0::path`. In both, the path is
        // everything after the last colon — splitting on ':' and taking the remainder is safe
        // because a cgroup path cannot itself contain a colon.
        let path = line.rsplit(':').next().unwrap_or(line);

        for seg in path.split('/') {
            if let Some(label) = recognize_segment(seg) {
                return Some(label);
            }
        }

        // `lxc/<name>` (cgroupfs driver) and `lxc.payload.<name>` (newer LXC) both name the
        // container in a segment rather than an opaque id; surface the human name as-is.
        if let Some(name) = lxc_name(path) {
            return Some(format!("lxc:{name}"));
        }

        // kubepods anywhere in the path means k8s even when no recognizable container id
        // scope is present (e.g. the pod-level slice). Last resort so a more specific
        // crio/containerd id above is preferred.
        if path.split('/').any(|s| s.starts_with("kubepods")) {
            return Some("k8s:?".into());
        }
    }
    None
}

/// Match a single path segment against the known systemd-scope shapes. The id inside a scope
/// is a 64-hex container id (or a `cri-containerd-<id>` / `crio-<id>` variant); we keep the
/// first 12 hex — the same short form `docker ps` shows — so labels are comparable by eye.
fn recognize_segment(seg: &str) -> Option<String> {
    // Strip the `.scope` / `.service` systemd suffix once; the runtime prefix is what matters.
    let body = seg.strip_suffix(".scope").unwrap_or(seg);

    // Order matters: `cri-containerd-` must be tested before a bare `containerd` check, and
    // both before the generic forms, so the most specific runtime label wins.
    if let Some(id) = body.strip_prefix("docker-") {
        return short_hex(id).map(|h| format!("docker:{h}"));
    }
    if let Some(id) = body.strip_prefix("cri-containerd-") {
        return short_hex(id).map(|h| format!("k8s:{h}"));
    }
    if let Some(id) = body.strip_prefix("crio-") {
        return short_hex(id).map(|h| format!("k8s:{h}"));
    }
    if let Some(id) = body.strip_prefix("containerd-") {
        return short_hex(id).map(|h| format!("k8s:{h}"));
    }
    if let Some(id) = body.strip_prefix("libpod-") {
        return short_hex(id).map(|h| format!("podman:{h}"));
    }
    None
}

/// The container name for the two LXC path layouts, or `None`. We only treat a segment as the
/// name when its predecessor is the `lxc` / `lxc.payload` marker, so an unrelated dir literally
/// called `lxc-foo` elsewhere in the path does not get mistaken for a container.
fn lxc_name(path: &str) -> Option<&str> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for (i, seg) in segs.iter().enumerate() {
        // `lxc/<name>`: the marker is its own segment.
        if *seg == "lxc" {
            if let Some(name) = segs.get(i + 1) {
                return non_empty(name);
            }
        }
        // `lxc.payload.<name>`: the name is glued onto the marker in one segment.
        if let Some(name) = seg.strip_prefix("lxc.payload.") {
            return non_empty(name);
        }
    }
    None
}

/// First 12 chars of a string that is entirely lowercase/uppercase hex of length ≥ 12.
/// Rejecting non-hex and too-short ids is what keeps `docker-<not an id>.scope` from
/// producing a confident-looking but fake `docker:` label.
fn short_hex(id: &str) -> Option<String> {
    if id.len() >= 12 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(id[..12].to_ascii_lowercase())
    } else {
        None
    }
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// Container identity for a live PID by reading `/proc/<pid>/cgroup`. Any IO error (PID gone,
/// foreign user, no procfs) reads as "host / unknown" → `None`; this never fails.
pub fn container_of(pid: u32) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_cgroup(&content)
}

/// Tracks per-PID CPU time across ticks to turn the kernel's *cumulative* counter into an
/// instantaneous rate. A single `/proc/<pid>/stat` read only gives total ticks consumed since
/// the process started; the rate is the delta between two reads over the wall time between them.
#[derive(Default)]
pub struct CpuTracker {
    /// pid → (cumulative utime+stime ticks, the instant we read them).
    seen: HashMap<u32, (u64, Instant)>,
}

impl CpuTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// CPU % (relative to one core) for `pid`, or `None`. The first sighting only establishes
    /// a baseline — there is no prior point to difference against — so it returns `None` by
    /// design rather than fabricating a since-boot average. Later sightings return the rate.
    /// A missing/unreadable stat (PID exited, foreign user) is a normal `None`.
    pub fn sample(&mut self, pid: u32) -> Option<f32> {
        let content = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = parse_stat_ticks(&content)?;
        let now = Instant::now();

        let prev = self.seen.insert(pid, (ticks, now));
        let (prev_ticks, prev_instant) = prev?; // first sighting → baseline only, no rate yet

        // A counter that went backwards means pid reuse re-created the process between reads;
        // checked_sub yields None and we make no claim rather than report a wild value.
        let delta_ticks = ticks.checked_sub(prev_ticks)?;
        let elapsed = now.duration_since(prev_instant).as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }

        let pct = (delta_ticks as f64 / USER_HZ / elapsed * 100.0) as f32;
        Some(pct.clamp(0.0, CPU_PCT_MAX))
    }

    /// Drop bookkeeping for PIDs no longer present so the map does not grow without bound over
    /// a long-running session (a busy host churns through thousands of short-lived PIDs).
    pub fn prune(&mut self, live_pids: &[u32]) {
        self.seen.retain(|pid, _| live_pids.contains(pid));
    }
}

/// Sum of `utime` + `stime` (fields 14 and 15, 1-indexed) from a `/proc/<pid>/stat` line.
///
/// The classic parsing trap: field 2 is `comm`, the executable name in parentheses, and it can
/// contain spaces AND parentheses — e.g. a thread named `(my proc) worker` yields
/// `1234 ((my proc) worker) S ...`. Splitting the whole line on whitespace therefore mis-counts
/// fields. The robust fix the kernel itself documents: `comm` is wrapped in the FIRST `(` and
/// the LAST `)`, so slice from after the last `)` and field-count the tail. After that `)` the
/// fields are fixed-position and space-separated, with `state` first — so utime/stime are the
/// 12th and 13th tokens of the tail (stat fields 14/15 = tail positions 12/13, since the tail
/// begins at field 3 `state`).
pub fn parse_stat_ticks(stat: &str) -> Option<u64> {
    // Everything after the last ')' is the parentheses-free tail starting at the `state` field.
    let tail = stat.rsplit_once(')')?.1;
    let mut fields = tail.split_whitespace();

    // Tail field 1 is `state`; stat field 14 (utime) is tail field 12, field 15 (stime) is 13.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    utime.checked_add(stime)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- cgroup parsing: realistic cgroup v2 content per runtime ----

    #[test]
    fn docker_scope_yields_short_id() {
        // systemd cgroup driver, cgroup v2: one `0::` line, docker scope under system.slice.
        let c = "0::/system.slice/docker-1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d.scope\n";
        assert_eq!(parse_cgroup(c).as_deref(), Some("docker:1a2b3c4d5e6f"));
    }

    #[test]
    fn cri_containerd_and_crio_map_to_k8s() {
        let containerd = "0::/kubepods.slice/kubepods-burstable.slice/\
            kubepods-burstable-pod123.slice/\
            cri-containerd-abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789.scope\n";
        assert_eq!(
            parse_cgroup(containerd).as_deref(),
            Some("k8s:abcdef012345")
        );

        let crio = "0::/kubepods.slice/kubepods-besteffort.slice/\
            crio-fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210.scope\n";
        assert_eq!(parse_cgroup(crio).as_deref(), Some("k8s:fedcba987654"));
    }

    #[test]
    fn kubepods_without_recognizable_id_is_unknown_pod() {
        // The pod-level slice with no container scope: we know it is k8s but not which pod.
        let c = "0::/kubepods.slice/kubepods-besteffort.slice/kubepods-besteffort-pod9f8e.slice\n";
        assert_eq!(parse_cgroup(c).as_deref(), Some("k8s:?"));
    }

    #[test]
    fn libpod_scope_yields_podman() {
        let c = "0::/machine.slice/\
            libpod-0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0.scope\n";
        assert_eq!(parse_cgroup(c).as_deref(), Some("podman:0f1e2d3c4b5a"));
    }

    #[test]
    fn lxc_both_path_layouts_surface_the_name() {
        // cgroupfs driver: `lxc/<name>`.
        let plain = "11:cpuset:/lxc/webserver\n10:devices:/lxc/webserver\n";
        assert_eq!(parse_cgroup(plain).as_deref(), Some("lxc:webserver"));
        // newer LXC: `lxc.payload.<name>`.
        let payload = "0::/lxc.payload.dbnode/system.slice/postgres.service\n";
        assert_eq!(parse_cgroup(payload).as_deref(), Some("lxc:dbnode"));
    }

    #[test]
    fn host_process_is_none() {
        // A plain systemd user-session process: no container marker anywhere.
        let c = "0::/user.slice/user-1000.slice/session-2.scope\n";
        assert_eq!(parse_cgroup(c), None);
        // The init/system path is likewise host.
        assert_eq!(parse_cgroup("0::/system.slice/sshd.service\n"), None);
    }

    #[test]
    fn garbage_and_decoy_ids_are_none() {
        assert_eq!(parse_cgroup(""), None);
        assert_eq!(parse_cgroup("not a cgroup file at all"), None);
        // `docker-` prefix but the id is not hex / too short → not a real container id.
        assert_eq!(
            parse_cgroup("0::/system.slice/docker-notanid.scope\n"),
            None
        );
        assert_eq!(
            parse_cgroup("0::/system.slice/docker-deadbeef.scope\n"),
            None
        );
    }

    // ---- /proc/<pid>/stat parsing: the comm-with-parens trap ----

    #[test]
    fn stat_ticks_handles_comm_with_spaces_and_parens() {
        // comm = `(my proc) worker` — contains ") (" and a leading paren. utime=111, stime=22.
        // Layout: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt
        //         majflt cmajflt utime stime ...
        let line = "1234 ((my proc) worker) S 1 1234 1234 0 -1 4194560 \
                    1000 0 5 0 111 22 0 0 20 0 1 0 9999 123456 789 ...\n";
        assert_eq!(parse_stat_ticks(line), Some(133));
    }

    #[test]
    fn stat_ticks_simple_comm() {
        // Plain comm with no parens: utime=50, stime=10 → 60.
        let line = "980 (python) R 1 980 980 0 -1 4194304 200 0 0 0 50 10 0 0 20 0 8 0 1000 0 0\n";
        assert_eq!(parse_stat_ticks(line), Some(60));
    }

    #[test]
    fn stat_ticks_rejects_truncated_or_garbage() {
        // No closing paren at all.
        assert_eq!(parse_stat_ticks("1234 (python S 1 1 1"), None);
        // Tail too short to reach utime/stime.
        assert_eq!(parse_stat_ticks("1234 (python) S 1 1"), None);
        // utime is non-numeric.
        let bad = "1 (x) S 1 1 1 0 -1 0 0 0 0 0 xx 22 0 0 20 0 1 0 1 0 0\n";
        assert_eq!(parse_stat_ticks(bad), None);
    }

    // ---- CpuTracker: baseline-then-rate semantics, pruning ----

    #[test]
    fn cpu_tracker_first_sighting_has_no_baseline() {
        // We cannot read a synthetic /proc here, but the pruning/baseline bookkeeping is
        // exercisable: a never-seen pid sampled from a real /proc still returns None the
        // first time. Sampling our own pid twice would race the scheduler, so we only assert
        // the no-baseline contract on the first read.
        let mut t = CpuTracker::new();
        let me = std::process::id();
        assert_eq!(
            t.sample(me),
            None,
            "first sighting establishes baseline only"
        );
    }

    #[test]
    fn cpu_tracker_prune_drops_dead_pids() {
        let mut t = CpuTracker::new();
        // Seed the map directly to test prune without depending on /proc timing.
        t.seen.insert(111, (10, Instant::now()));
        t.seen.insert(222, (20, Instant::now()));
        t.seen.insert(333, (30, Instant::now()));
        t.prune(&[222]);
        assert!(!t.seen.contains_key(&111));
        assert!(t.seen.contains_key(&222));
        assert!(!t.seen.contains_key(&333));
    }
}
