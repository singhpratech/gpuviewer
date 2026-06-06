//! gpuviewer — the GPU flight recorder.
//!
//! Modes:
//!   gpuviewer                 interactive TUI
//!   gpuviewer --json          stream one NDJSON frame per tick to stdout
//!   gpuviewer --json --once   print a single frame and exit
//!
//! Flags: --mock (use only the simulated GPUs), --interval <ms> (default 1000, min 100).

mod app;
mod collector;
mod ui;

use std::time::Duration;

use anyhow::{bail, Result};
use collector::{Collector, Engine};

struct Args {
    json: bool,
    once: bool,
    mock: bool,
    interval: Duration,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        json: false,
        once: false,
        mock: false,
        interval: Duration::from_millis(1000),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => args.json = true,
            "--once" => args.once = true,
            "--mock" => args.mock = true,
            "--interval" => {
                let v = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--interval needs a value (ms)"))?;
                let ms: u64 = v
                    .parse()
                    .map_err(|e| anyhow::anyhow!("--interval: invalid value {v:?}: {e}"))?;
                if ms < 100 {
                    eprintln!("gpuviewer: --interval {ms} clamped to 100ms");
                }
                args.interval = Duration::from_millis(ms.max(100));
            }
            "--version" | "-V" => {
                println!("gpuviewer {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "gpuviewer — the GPU flight recorder\n\n\
                     USAGE: gpuviewer [--json [--once]] [--mock] [--interval <ms>] [--help] [--version]\n\n\
                     --json          stream one NDJSON frame per tick to stdout\n\
                     --once          with --json: print a single frame and exit\n\
                     --mock          use ONLY the simulated GPUs (deterministic; also the fallback when no GPU is found)\n\
                     --interval <ms> sampling interval, default 1000, minimum 100\n\
                     --version, -V   print version and exit\n\
                     --help, -h      show this help"
                );
                std::process::exit(0);
            }
            other => bail!("unknown flag: {other} (see --help)"),
        }
    }
    Ok(args)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let engine = Engine::new(args.mock);

    if args.json {
        run_json(engine, args.interval, args.once)
    } else {
        let collector = Collector::start(engine, args.interval);
        app::App::new(collector).run()
    }
}

fn run_json(mut engine: Engine, interval: Duration, once: bool) -> Result<()> {
    loop {
        let frame = engine.tick();
        println!("{}", serde_json::to_string(&frame)?);
        if once {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}
