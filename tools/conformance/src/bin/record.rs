//! Records goldens from a running emulator (normally the Java one, started with `--testing`):
//!
//!     cargo run -p conformance --bin record-conformance -- [--firestore] http://localhost:8081 [scenario...]
//!
//! With `--firestore` (an emulator started with `--firestore_in_datastore_mode`) the goldens go to
//! `tests/conformance/java-firestore`.

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let firestore = args.next_if(|a| a == "--firestore").is_some();
    let dir = if firestore {
        conformance::firestore_golden_dir()
    } else {
        conformance::golden_dir()
    };
    let base = args
        .next()
        .context("usage: record-conformance <base-url> [scenario...]")?;
    let mut names: Vec<String> = args.collect();
    if names.is_empty() {
        names = conformance::scenario_names()?;
    }
    std::fs::create_dir_all(&dir)?;
    for name in names {
        let steps = conformance::load_scenario(&name)?;
        let exchanges =
            conformance::run(&base, &steps).with_context(|| format!("scenario {name}"))?;
        let path = dir.join(format!("{name}.golden"));
        std::fs::write(&path, conformance::to_golden(&exchanges))?;
        let statuses: Vec<String> = exchanges.iter().map(|e| e.status.to_string()).collect();
        println!("{name}: {}", statuses.join(" "));
    }
    Ok(())
}
