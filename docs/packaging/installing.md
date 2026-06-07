# Installing gpuviewer

Prebuilt binaries are published on [GitHub Releases](https://github.com/singhpratech/gpuviewer/releases).
They are **not code-signed** — signing costs (Apple Developer ID, Windows certificates)
are not justified yet. The integrity story instead: per-target `SHA256SUMS-<target>`
files plus GitHub build-provenance attestations on every artifact
([verify](#verify-what-you-downloaded)). Platforms ship per the roadmap: Linux from v1,
Windows from v1.5, macOS Apple Silicon from v2.

## One-line install

```sh
# Linux (x86_64) / macOS (Apple Silicon)
curl -fsSL https://raw.githubusercontent.com/singhpratech/gpuviewer/main/install.sh | sh
```

```powershell
# Windows (x86_64)
irm https://raw.githubusercontent.com/singhpratech/gpuviewer/main/install.ps1 | iex
```

Both scripts resolve the latest release tag, download the matching archive **and** its
`SHA256SUMS-<target>`, verify the checksum, and only then install a single binary. They
fail closed on any mismatch and never edit your shell profile or PATH.

- **install.sh** installs to `~/.local/bin` (override with `GPUVIEWER_BIN_DIR`); pin a
  version with `GPUVIEWER_VERSION`. Prints a PATH hint if the bin dir isn't on `PATH`,
  but won't modify it for you.
- **install.ps1** installs to `%LOCALAPPDATA%\Programs\gpuviewer` (override with `-BinDir`);
  pin with `-Version`. Idempotently appends the dir to the user-scope `Path`.

macOS bonus: `curl` does **not** set the `com.apple.quarantine` attribute, so the
script path sidesteps the Gatekeeper friction described under
[macOS](#macos-apple-silicon--from-v2) — no "Open Anyway" prompt, no `xattr` dance.

## Linux (x86_64, glibc 2.35 or newer)

Built on Ubuntu 22.04, so the binary needs glibc ≥ 2.35 (Ubuntu 22.04+, Debian 12+,
Fedora 36+). RHEL 9 ships glibc 2.34 — [build from source](#build-from-source-always-works)
there.

- **tar.gz**: extract and run — `tar xzf gpuviewer-<ver>-x86_64-unknown-linux-gnu.tar.gz && ./gpuviewer-<ver>-x86_64-unknown-linux-gnu/gpuviewer`
- **deb**: `sudo apt install ./gpuviewer_<ver>-1_amd64.deb`
- **rpm**: `sudo dnf install ./gpuviewer-<ver>-1.x86_64.rpm`
- **AppImage**: one self-contained file that runs on any distro meeting the same glibc
  ≥ 2.35 floor (it does **not** bundle glibc). `chmod +x gpuviewer-<ver>-x86_64.AppImage`
  first, then either run it directly (needs `libfuse2` installed), or run it without FUSE:

  ```sh
  chmod +x gpuviewer-<ver>-x86_64.AppImage
  ./gpuviewer-<ver>-x86_64.AppImage --appimage-extract-and-run --version
  ```

The deb/rpm/AppImage deliberately bundle **no** NVIDIA driver pieces: gpuviewer dlopens
`libnvidia-ml.so.1` at runtime and degrades gracefully when it is absent (AMD and Intel
need no driver package at all — they are read via sysfs/fdinfo).

## Windows (x86_64) — from v1.5

Unzip `gpuviewer-<ver>-x86_64-pc-windows-msvc.zip` and run `gpuviewer.exe` from a
terminal. SmartScreen will flag the unsigned exe on first launch: choose
**More info → Run anyway**.

## macOS (Apple Silicon) — from v2

Browser downloads get the `com.apple.quarantine` attribute (Archive Utility propagates
it into the extracted files), and macOS Sequoia 15.1+ removed the ctrl-click
"Open anyway" bypass. Pick one:

1. Approve the binary under **System Settings → Privacy & Security → "Open Anyway"**
   after the first blocked launch.
2. Clear the quarantine attribute:

   ```sh
   xattr -d com.apple.quarantine ./gpuviewer
   ```

3. Download with curl or wget instead of a browser — neither sets the quarantine
   attribute:

   ```sh
   curl -LO <release asset URL>
   ```

## Verify what you downloaded

```sh
sha256sum -c --ignore-missing SHA256SUMS-<target>   # shasum -a 256 -c on macOS
gh attestation verify <file> -R singhpratech/gpuviewer
```

## Build from source (always works)

Needs Rust 1.95+ (the declared MSRV); no GPU, no vendor SDK, no system SQLite — all
dependencies build from source.

```sh
cargo install --locked --git https://github.com/singhpratech/gpuviewer gpuviewer-tui
```

The installed binary is named `gpuviewer`. Or clone and `cargo build --release` —
the binary lands in `target/release/gpuviewer`.
