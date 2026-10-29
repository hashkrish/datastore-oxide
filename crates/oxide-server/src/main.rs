//! `datastore-oxide`: a drop-in replacement for the Java `cloud_datastore_emulator`.

mod cli;
mod create;
mod server;

use std::io::IsTerminal;
use std::process::ExitCode;

use cli::Command;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OXIDE_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match cli::parse(&args) {
        Command::Exit { stdout, code } => {
            print!("{stdout}");
            code
        }
        Command::License => {
            // Ignore a closed pipe (`license | head`).
            let _ = std::io::Write::write_all(&mut std::io::stdout(), license_text().as_bytes());
            0
        }
        Command::Create(cfg) => create::run(&cfg).unwrap_or_else(|e| {
            eprintln!("{e:#}");
            1
        }),
        Command::Start(cfg) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();
            match runtime
                .map_err(anyhow::Error::from)
                .and_then(|rt| rt.block_on(server::run(cfg)))
            {
                Ok(code) => code,
                Err(e) => {
                    println!("Exiting due to exception: {e:#}");
                    1
                }
            }
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// This project's license and notice, then the third-party licenses (regenerate them with
/// `tools/gen-licenses.py`).
fn license_text() -> String {
    format!(
        "datastore-oxide {}\nLicensed under the Apache License, Version 2.0 (see LICENSE).\n\n{}\n{}",
        env!("CARGO_PKG_VERSION"),
        include_str!("../../../NOTICE"),
        include_str!("third_party_licenses.txt")
    )
}
