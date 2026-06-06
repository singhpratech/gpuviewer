//! gpuviewer — the GPU flight recorder.
//!
//! Modes:
//!   gpuviewer                 interactive TUI
//!   gpuviewer --json          stream one NDJSON frame per tick to stdout
//!   gpuviewer --json --once   print a single frame and exit
//!
//! Flags: --mock (force the mock backend), --interval <ms> (default 1000).

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
                let ms: u64 = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--interval needs a value (ms)"))?
                    .parse()?;
                args.interval = Duration::from_millis(ms.max(100));
            }
            "--help" | "-h" => {
                println!(
                    "gpuviewer — the GPU flight recorder\n\n\
                     USAGE: gpuviewer [--json [--once]] [--mock] [--interval <ms>]\n\n\
                     --json          stream one NDJSON frame per tick to stdout\n\
                     --once          with --json: print a single frame and exit\n\
                     --mock          force the mock backend (also used when no GPU is found)\n\
                     --interval <ms> sampling interval, default 1000"
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
