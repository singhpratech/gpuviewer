//! Embed the Windows .exe icon (docs/design/cross-platform.md §6/§8). Harmless on
//! every other OS: the non-Windows-host stub below compiles with ZERO
//! build-dependencies, so this file is safe to land before the integrator adds the
//! `winresource` build-dependency to Cargo.toml.

/// Repo-relative path to the icon, resolved from this crate's manifest dir
/// (build-script cwd is the crate root, but absolute-from-manifest is unambiguous).
const ICO_RELATIVE: &str = "../../docs/assets/icon/gpuviewer.ico";

fn main() {
    let ico = format!(
        "{}/{ICO_RELATIVE}",
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR")
    );
    println!("cargo:rerun-if-changed={ico}");

    // Gate on the TARGET via env var, not #[cfg]: build scripts are compiled for the
    // HOST, so `#[cfg(target_os = "windows")]` here would answer "is the build
    // machine Windows", not "are we building a Windows binary" (design §8).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_icon(&ico);
    }
}

// `winresource` is a [target.'cfg(target_os = "windows")'.build-dependencies] entry,
// so it is absent from the build graph on non-Windows targets — the code naming it
// must therefore be compiled out somewhere. The only cfg available to a build script
// is the HOST's, which is correct for the case that matters: release/CI Windows
// builds run on windows-latest (host == target), where the real branch compiles and
// rc.exe is present. Known accepted gap: cross-compiling Linux -> Windows skips the
// icon embed (stub wins on a Linux host) — fine, §5.6 limits local cross-builds to
// the core crate anyway, and shipping Windows zips are built on Windows runners.
#[cfg(windows)]
fn embed_icon(ico: &str) {
    winresource::WindowsResource::new()
        .set_icon(ico)
        .compile()
        .expect("winresource: failed to embed gpuviewer.ico");
}

#[cfg(not(windows))]
fn embed_icon(_ico: &str) {}
