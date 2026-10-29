//! Records project cases (`tests/conformance/projects/*.case`) against the Java emulator, run in a
//! JRE container with podman, into `tests/conformance/java-projects/*.golden`.
//!
//!     cargo run -p conformance --bin record-projects -- [case...]
//!
//! `EMU` overrides the emulator directory (default
//! `<gcloud SDK root>/platform/cloud-datastore-emulator`); `RECORD_PORT` the host port
//! (default 18110).

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use conformance::project::{self, Launcher};

const GUEST_DIR: &str = "/work/proj";
const IMAGE: &str = "docker.io/library/eclipse-temurin:21-jre";

struct Podman {
    emu: PathBuf,
    work: PathBuf,
    port: u16,
    name: String,
}

impl Launcher for Podman {
    fn start(&mut self, args: &[String]) -> Result<String> {
        let status = Command::new("podman")
            .args(["run", "-d", "--rm", "--name", &self.name])
            .args(["-p", &format!("{}:8081", self.port)])
            .args(["-v", &format!("{}:/emu:ro", self.emu.display())])
            .args(["-v", &format!("{}:/work:Z", self.work.display())])
            .args([IMAGE, "java", "-cp", "/emu/CloudDatastore.jar"])
            .args([
                "com.google.cloud.datastore.emulator.CloudDatastore",
                "start",
            ])
            .args(["--host=0.0.0.0", "--port=8081"])
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()?;
        if !status.success() {
            bail!("podman run failed");
        }
        let base = format!("http://localhost:{}", self.port);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if ureq::get(&base).call().is_ok_and(|r| r.status() == 200) {
                return Ok(base);
            }
            if Instant::now() > deadline {
                let logs = Command::new("podman").args(["logs", &self.name]).output()?;
                bail!(
                    "emulator did not start:\n{}{}",
                    String::from_utf8_lossy(&logs.stdout),
                    String::from_utf8_lossy(&logs.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn wait_exit(&mut self) -> Result<()> {
        // `podman wait` fails once the --rm container is gone, which also means it exited.
        let _ = Command::new("podman")
            .args(["wait", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        while Command::new("podman")
            .args(["container", "exists", &self.name])
            .status()?
            .success()
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

/// The Cloud SDK install directory, as reported by `gcloud info`.
fn gcloud_sdk_root() -> Result<PathBuf> {
    let out = Command::new("gcloud")
        .args(["info", "--format=value(installation.sdk_root)"])
        .output()
        .context("running gcloud (set EMU to the emulator directory instead)")?;
    let root = String::from_utf8(out.stdout)?.trim().to_owned();
    if !out.status.success() || root.is_empty() {
        bail!("gcloud info found no SDK root; set EMU to the emulator directory");
    }
    Ok(PathBuf::from(root))
}

fn main() -> Result<()> {
    let mut names: Vec<String> = std::env::args().skip(1).collect();
    if names.is_empty() {
        names = project::project_names()?;
    }
    let emu = match std::env::var("EMU") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => gcloud_sdk_root()?.join("platform/cloud-datastore-emulator"),
    };
    let port = std::env::var("RECORD_PORT").map_or(Ok(18110), |p| p.parse())?;
    std::fs::create_dir_all(project::project_golden_dir())?;
    for name in names {
        let case = project::load_project(&name)?;
        let work = std::env::temp_dir().join(format!("oxide-record-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let host_dir = work.join("proj");
        std::fs::create_dir_all(&host_dir)?;
        let mut podman = Podman {
            emu: emu.clone(),
            work: work.clone(),
            port,
            name: format!("oxide-record-{}", std::process::id()),
        };
        let exchanges = project::run_project(&case, &host_dir, GUEST_DIR, &mut podman)
            .with_context(|| format!("case {name}"))?;
        let statuses: Vec<String> = exchanges.iter().map(|e| e.status.to_string()).collect();
        println!("{name}: {}", statuses.join(" "));
        std::fs::write(
            project::project_golden_dir().join(format!("{name}.golden")),
            conformance::to_golden(&exchanges),
        )?;
        let _ = std::fs::remove_dir_all(&work);
    }
    Ok(())
}
