//! One-off ground-truth probe for the macOS backend's CI assertions — design §4.5/§5.5
//! (`docs/design/cross-platform.md`). Run by the manual `macos-probe` workflow job (which
//! also dumps `ioreg -r -c IOAccelerator` for the Tier B side); run it on real Macs per
//! chip/macOS release to capture committed fixtures.
//!
//! It answers exactly one question: **what does this machine's IOReport actually
//! expose?** — printed as `channel|<group>|<subgroup>|<name>|<unit>` lines, the same
//! format `apple::parse::ChannelDesc` reads, so output can be committed verbatim under
//! `crates/core/tests/fixtures/ioreport/` (see that README). Per §4.5, the paravirt
//! guest's absences are *inference until this probe says so* — never bake a CI `None`
//! assertion without this output.
//!
//! WHY this file is deliberately self-contained (std-only, hand-rolled `dlopen`/`dlsym`
//! and CoreFoundation externs, no `gpuviewer_core::apple` import, no crate deps):
//! - it must compile on every CI leg (`clippy --all-targets -D warnings`) even before the
//!   `apple` feature/module wiring lands — a probe that can't build can't probe;
//! - probing the private dylib is the point, so the probe must not depend on the gated
//!   Tier C plumbing it exists to de-risk (design §4.6 keeps that plumbing unbuilt);
//! - CoreFoundation is a public OS framework — direct externs are fine under the
//!   no-vendor-SDK rule (its target is vendor SDKs with soname churn, not the OS).
//!
//! Symbol inventory mirrors macmon's `sources.rs` (MIT) — the prior art proving this
//! stack sudoless on current macOS. Every symbol is looked up individually and reported
//! as present/absent: a missing symbol is a *finding*, never a crash.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macos_probe: macOS-only ground-truth probe; nothing to do on this OS.");
}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_int, c_void, CString};

    // dlopen/dlsym live in libSystem, which every macOS binary links — no manifest dep.
    extern "C" {
        fn dlopen(path: *const c_char, flag: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    /// RTLD_NOW from darwin's <dlfcn.h>. Resolve everything up front: a probe wants the
    /// load failure here, loudly, not on first call.
    const RTLD_NOW: c_int = 2;

    // CoreFoundation: public OS framework (see module header for the no-vendor-SDK note).
    // CFIndex is a signed long (isize on LP64 darwin); Boolean is an unsigned char.
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFStringGetCString(s: *const c_void, buf: *mut c_char, len: isize, encoding: u32) -> u8;
    }
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    /// CFStringRef → String; NULL or conversion failure → empty string (the fixture
    /// format treats empty fields as legitimate — e.g. unit-less channels).
    unsafe fn cfstring_to_string(s: *const c_void) -> String {
        if s.is_null() {
            return String::new();
        }
        let mut buf = [0 as c_char; 512];
        if CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        ) == 0
        {
            return String::new();
        }
        std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }

    /// IOReport entry points the probe types and calls (enumeration only).
    type FnCopyAllChannels = unsafe extern "C" fn(u64, u64) -> *const c_void;
    type FnChannelGetStr = unsafe extern "C" fn(*const c_void) -> *const c_void;

    /// The full Tier C inventory (macmon's set). The probe only *calls* the enumeration
    /// five; the rest are presence-checked so the §4.6 unfreeze knows what this
    /// machine/OS actually exports before any sampling code is written.
    const PRESENCE_ONLY_SYMBOLS: &[&str] = &[
        "IOReportCreateSubscription",
        "IOReportCreateSamples",
        "IOReportCreateSamplesDelta",
        "IOReportStateGetCount",
        "IOReportStateGetNameForIndex",
        "IOReportStateGetResidency",
        "IOReportSimpleGetIntegerValue",
    ];

    pub(super) fn run() {
        println!("== macos_probe: IOReport ground truth (design §4.5/§5.5) ==");

        // NB: on modern macOS the dylib often has no file on disk (dyld shared cache),
        // so dlopen — not a filesystem check — is the only honest presence test.
        let path = CString::new("/usr/lib/libIOReport.dylib").expect("static path");
        let handle = unsafe { dlopen(path.as_ptr(), RTLD_NOW) };
        if handle.is_null() {
            // The load-bearing finding for the gate: record it and exit cleanly.
            println!("libIOReport: ABSENT (dlopen failed) — Tier C has no substrate here");
            return;
        }
        println!("libIOReport: present (dlopen ok)");

        let sym = |name: &str| -> *mut c_void {
            let c = CString::new(name).expect("static symbol name");
            unsafe { dlsym(handle, c.as_ptr()) }
        };

        for name in PRESENCE_ONLY_SYMBOLS {
            let p = sym(name);
            println!(
                "symbol|{name}|{}",
                if p.is_null() { "ABSENT" } else { "present" }
            );
        }

        // Enumeration set — each individually optional, absence reported not fatal.
        let copy_all = sym("IOReportCopyAllChannels");
        let get_group = sym("IOReportChannelGetGroup");
        let get_subgroup = sym("IOReportChannelGetSubGroup");
        let get_name = sym("IOReportChannelGetChannelName");
        let get_unit = sym("IOReportChannelGetUnitLabel");
        for (name, p) in [
            ("IOReportCopyAllChannels", copy_all),
            ("IOReportChannelGetGroup", get_group),
            ("IOReportChannelGetSubGroup", get_subgroup),
            ("IOReportChannelGetChannelName", get_name),
            ("IOReportChannelGetUnitLabel", get_unit),
        ] {
            println!(
                "symbol|{name}|{}",
                if p.is_null() { "ABSENT" } else { "present" }
            );
        }
        if copy_all.is_null()
            || get_group.is_null()
            || get_subgroup.is_null()
            || get_name.is_null()
            || get_unit.is_null()
        {
            println!("channel enumeration skipped: required symbol(s) missing (see above)");
            return;
        }

        // SAFETY: signatures mirror macmon's reverse-engineered prototypes; all pointers
        // are null-checked before use; only the one Copy-rule object we own is released
        // (the per-channel Get-rule strings are deliberately leaked — over-releasing a
        // misattributed object would crash this one-shot probe for nothing).
        unsafe {
            let copy_all: FnCopyAllChannels = std::mem::transmute(copy_all);
            let get_group: FnChannelGetStr = std::mem::transmute(get_group);
            let get_subgroup: FnChannelGetStr = std::mem::transmute(get_subgroup);
            let get_name: FnChannelGetStr = std::mem::transmute(get_name);
            let get_unit: FnChannelGetStr = std::mem::transmute(get_unit);

            let all = copy_all(0, 0);
            if all.is_null() {
                println!("IOReportCopyAllChannels returned NULL — no channels visible");
                return;
            }
            let key = CString::new("IOReportChannels").expect("static key");
            let cfkey = CFStringCreateWithCString(
                std::ptr::null(),
                key.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            );
            let channels = CFDictionaryGetValue(all, cfkey);
            if channels.is_null() {
                println!("channels dict has no IOReportChannels array — dump it via ioreg");
                CFRelease(all);
                return;
            }
            let n = CFArrayGetCount(channels);
            println!("== {n} channels (fixture format; commit verbatim per README) ==");
            for i in 0..n {
                let item = CFArrayGetValueAtIndex(channels, i);
                if item.is_null() {
                    continue;
                }
                // Same line shape as apple::parse::ChannelDesc::to_line — pinned there.
                println!(
                    "channel|{}|{}|{}|{}",
                    cfstring_to_string(get_group(item)),
                    cfstring_to_string(get_subgroup(item)),
                    cfstring_to_string(get_name(item)),
                    cfstring_to_string(get_unit(item)),
                );
            }
            CFRelease(all);
        }
    }
}
