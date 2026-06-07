#!/bin/sh
# gpuviewer one-line installer — Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/singhpratech/gpuviewer/main/install.sh | sh
#
# What it does, in order: detect OS/arch -> resolve the latest GitHub release (or
# $GPUVIEWER_VERSION) -> download the matching artifact AND its SHA256SUMS file ->
# verify the checksum -> install a single `gpuviewer` binary into the bin dir ->
# tell you if that dir is not on PATH. Nothing else: no sudo, no root, no config
# files, no shell-profile edits behind your back.
#
# Environment overrides:
#   GPUVIEWER_VERSION   install this tag (e.g. v0.1.0) instead of the latest release
#   GPUVIEWER_BIN_DIR   install dir (default: ~/.local/bin on Linux, ~/.local/bin or
#                       /usr/local/bin if it is user-writable on macOS)
#
# Why curl|sh is SAFER here than a browser download on macOS: curl sets no
# com.apple.quarantine attribute, so Gatekeeper does not block the unsigned binary
# (docs/packaging/installing.md). The checksum is still verified against the
# release's SHA256SUMS file before anything is installed.
#
# POSIX sh on purpose — runs under dash/ash/bash/zsh-as-sh on a fresh box.
set -eu

REPO="singhpratech/gpuviewer"
NAME="gpuviewer"

say()  { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- platform detection -------------------------------------------------------------------
os=$(uname -s)
arch=$(uname -m)
case "$arch" in
  arm64) arch=aarch64 ;;          # macOS spells aarch64 "arm64"
esac

case "$os" in
  Linux)
    case "$arch" in
      x86_64) target="x86_64-unknown-linux-gnu" ;;
      *) fail "no prebuilt Linux $arch binary yet (only x86_64) — build from source:
  cargo install --locked --git https://github.com/$REPO gpuviewer-tui" ;;
    esac
    ;;
  Darwin)
    case "$arch" in
      aarch64) target="aarch64-apple-darwin" ;;
      *) fail "no prebuilt macOS $arch binary (Apple Silicon only — Intel Macs must build from source):
  cargo install --locked --git https://github.com/$REPO gpuviewer-tui" ;;
    esac
    ;;
  *) fail "unsupported OS: $os (Linux and macOS only; Windows uses install.ps1)" ;;
esac

# A scratch dir for everything (downloads + the API status capture below). Created here so
# the version-resolution step can use it too; cleaned up on any exit.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# --- tools ---------------------------------------------------------------------------------
# api_get URL writes the response body to "$work/api_body" and the HTTP status to api_status.
# Status is kept separate from the body so "no release yet" (404) is told apart from a
# rate-limit (403) or a transport failure (network down → status 000) — conflating them
# would misdiagnose a temporary outage as "build from source".
api_status=
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  api_get() {
    api_status=$(curl -sS -o "$work/api_body" -w '%{http_code}' "$1" 2>/dev/null) \
      || { api_status=000; : > "$work/api_body"; return 1; }
  }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
  api_get() {
    # wget exits non-zero on HTTP error; --server-response leaks the status onto stderr.
    wget -qO "$work/api_body" --server-response "$1" 2>"$work/api_hdr" || true
    api_status=$(sed -n 's/.*HTTP\/[0-9.]* \([0-9][0-9][0-9]\).*/\1/p' "$work/api_hdr" | tail -n1)
    [ -n "$api_status" ] || api_status=000
  }
else
  fail "need curl or wget"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha_check() { (cd "$1" && sha256sum -c --ignore-missing "$2"); }
elif command -v shasum >/dev/null 2>&1; then
  sha_check() { (cd "$1" && shasum -a 256 -c --ignore-missing "$2"); }
else
  fail "need sha256sum or shasum to verify the download"
fi

# --- resolve version ------------------------------------------------------------------------
build_hint="Build from source instead:
  cargo install --locked --git https://github.com/$REPO gpuviewer-tui"
if [ -n "${GPUVIEWER_VERSION:-}" ]; then
  tag="$GPUVIEWER_VERSION"
else
  # The "latest" REST endpoint needs no auth. It returns 404 ONLY when no release has
  # ever been published; 403 means the unauthenticated rate limit; 000 means we never
  # reached GitHub. Each gets its own message so the user isn't told to build from source
  # when the real problem is "try again in an hour" or "check your network".
  # `|| true`: a transport failure returns non-zero but sets api_status=000, which the
  # case below handles — don't let `set -e` abort before we can report it.
  api_get "https://api.github.com/repos/$REPO/releases/latest" || true
  case "$api_status" in
    200)
      tag=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$work/api_body" | head -n1)
      [ -n "$tag" ] || fail "GitHub returned 200 but no tag_name — unexpected; $build_hint" ;;
    404) fail "no published release found yet — gpuviewer binaries appear with the first tagged release.
$build_hint" ;;
    403) fail "GitHub API rate limit hit (HTTP 403). Wait a bit and retry, or pin a known tag:
  GPUVIEWER_VERSION=v0.1.0 sh install.sh" ;;
    000) fail "could not reach the GitHub API (network/DNS/proxy?). Check connectivity, or pin a tag with GPUVIEWER_VERSION." ;;
    *)   fail "unexpected GitHub API response (HTTP $api_status). $build_hint" ;;
  esac
fi
version="${tag#v}"
say "installing $NAME $tag ($target)"

# --- download + verify -----------------------------------------------------------------------
asset="$NAME-$version-$target.tar.gz"
sums="SHA256SUMS-$target"
base="https://github.com/$REPO/releases/download/$tag"

say "downloading $asset ..."
fetch "$base/$asset" "$work/$asset" || fail "download failed: $base/$asset"
fetch "$base/$sums"  "$work/$sums"  || fail "download failed: $base/$sums (checksums are mandatory)"

say "verifying checksum ..."
sha_check "$work" "$sums" >/dev/null || fail "SHA256 verification FAILED — refusing to install"

tar -xzf "$work/$asset" -C "$work" || fail "could not extract $asset (corrupt archive?)"
bin="$work/$NAME-$version-$target/$NAME"
[ -f "$bin" ] || fail "archive did not contain the expected binary"

# --- install ---------------------------------------------------------------------------------
bin_dir="${GPUVIEWER_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$bin_dir"
install -m 0755 "$bin" "$bin_dir/$NAME"
say "installed: $bin_dir/$NAME"

# PATH hint, never a shell-profile edit: tell the user exactly what to add, once.
case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say ""
     say "note: $bin_dir is not on your PATH. Add this to your shell profile:"
     say "  export PATH=\"$bin_dir:\$PATH\"" ;;
esac

say ""
say "run: $NAME            (live TUI — already recording)"
say "     $NAME demo       (8h simulated incident, opens at the throttle onset)"
