//! Termination signals → a cooperative shutdown flag.
//!
//! Without this, the recommended always-on setup (`gpuviewer --json` under the shipped
//! systemd user unit) lost its recording tail on every `systemctl stop`: SIGTERM's
//! default action kills the process outright, so the partial 10s/1m buckets and the
//! session's `recording_stopped` mark never reached the store, and the NEXT session
//! narrated a clean stop as an unclean one ("previous session ended uncleanly") — a
//! confidently-wrong fact about the recorder itself. The same held for an SSH session
//! dropping under the TUI (SIGHUP) and Ctrl-C in `--json` mode (SIGINT).
//!
//! Policy: the first signal sets [`requested`]; the live loops poll it at their sleep
//! slices and take the same clean-exit path a `q` or a closed stdout takes (stop mark,
//! tail flush, terminal restored). A SECOND signal of the same kind while the first is
//! still being honoured exits immediately with status 1 — a tick wedged inside a hung
//! driver must never make the process unkillable by Ctrl-C.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Install the handlers once, for the live modes (`--json` and the TUI). Read-only
/// subcommands (`report`, `export`, `view`) keep the default dispositions: they hold no
/// recording, so dying on a signal loses nothing. Failure to register is not fatal — the
/// process simply keeps the default (kill) behaviour, which is what it had before.
pub fn install() {
    let flag = FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)));
    #[cfg(unix)]
    let signals: &[i32] = &[
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ];
    #[cfg(windows)]
    let signals: &[i32] = &[signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT];
    for &sig in signals {
        // Order matters: the conditional shutdown must be registered BEFORE the flag
        // setter, so the first signal only sets the flag and the second (flag already
        // true) exits.
        let ok = signal_hook::flag::register_conditional_shutdown(sig, 1, Arc::clone(flag))
            .and_then(|_| signal_hook::flag::register(sig, Arc::clone(flag)));
        if let Err(e) = ok {
            eprintln!("gpuviewer: could not install handler for signal {sig}: {e}");
        }
    }
}

/// True once a termination signal arrived. Always false when [`install`] never ran
/// (tests, read-only subcommands).
pub fn requested() -> bool {
    FLAG.get().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Sleep for `d` in short slices, returning early (`true`) as soon as a shutdown was
/// requested — so a backoff-stretched 10s cadence cannot delay a `systemctl stop` past
/// its TimeoutStopSec.
pub fn sleep_or_shutdown(d: std::time::Duration) -> bool {
    const SLICE: std::time::Duration = std::time::Duration::from_millis(100);
    let deadline = std::time::Instant::now() + d;
    loop {
        if requested() {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep((deadline - now).min(SLICE));
    }
}
