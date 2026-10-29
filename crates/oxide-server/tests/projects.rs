//! Replays the project cases (`tests/conformance/projects`: index files, startup flags, restarts)
//! against our binary, each in a fresh project directory, and compares with the Java emulator's
//! recordings (`tests/conformance/java-projects`). `CASE=name` runs one case.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use conformance::project::{self, Launcher};

struct Oxide {
    child: Option<Child>,
    cwd: PathBuf,
}

impl Launcher for Oxide {
    fn start(&mut self, args: &[String]) -> Result<String> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
            .arg("start")
            .arg("--port=0")
            .args(args)
            .current_dir(&self.cwd)
            .env("OXIDE_LOG", "off")
            .stdout(Stdio::piped())
            .spawn()?;
        let mut lines = BufReader::new(child.stdout.take().context("stdout")?).lines();
        let first = lines.next().context("no banner")??;
        let port: u16 = first
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .with_context(|| format!("unexpected first line: {first}"))?;
        std::thread::spawn(move || lines.for_each(drop));
        self.child = Some(child);
        Ok(format!("http://127.0.0.1:{port}"))
    }

    fn wait_exit(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while child.try_wait()?.is_none() {
                if std::time::Instant::now() > deadline {
                    child.kill()?;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        Ok(())
    }
}

impl Drop for Oxide {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }
}

/// Steps where we deliberately differ (shared with the REST scenarios' allowlist).
fn allowlist() -> std::collections::HashSet<(String, usize)> {
    let text = std::fs::read_to_string(
        conformance::workspace_root().join("tests/conformance/allowlist.txt"),
    )
    .unwrap();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut parts = l.split_whitespace();
            (
                parts.next().unwrap().to_string(),
                parts.next().unwrap().parse().unwrap(),
            )
        })
        .collect()
}

#[test]
fn projects_match_java_goldens() {
    let only = std::env::var("CASE").ok();
    let allowed = allowlist();
    let mut failures = Vec::new();
    let mut checked = 0;
    for name in project::project_names().unwrap() {
        if only.as_ref().is_some_and(|o| o != &name) {
            continue;
        }
        let golden_path = project::project_golden_dir().join(format!("{name}.golden"));
        let Ok(golden) = std::fs::read_to_string(&golden_path) else {
            failures.push(format!(
                "{name}: no golden (record it with record-projects)"
            ));
            continue;
        };
        let golden = conformance::parse_golden(&golden).unwrap();
        let case = project::load_project(&name).unwrap();
        let work =
            std::env::temp_dir().join(format!("oxide-project-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let dir = work.join("proj");
        std::fs::create_dir_all(&dir).unwrap();
        let dir_text = dir.to_str().unwrap().to_string();
        let mut oxide = Oxide {
            child: None,
            cwd: work.clone(),
        };
        let actual = match project::run_project(&case, &dir, &dir_text, &mut oxide) {
            Ok(actual) => actual,
            Err(e) => {
                failures.push(format!("{name}: run failed: {e:#}"));
                continue;
            }
        };
        let _ = std::fs::remove_dir_all(&work);
        let want = project::normalize_project(&golden, "/work/proj");
        let got = project::normalize_project(&actual, &dir_text);
        for (i, (w, g)) in golden.iter().zip(&actual).enumerate() {
            checked += 1;
            if allowed.contains(&(name.clone(), i + 1)) {
                continue;
            }
            if w.status != g.status || want[i] != got[i] {
                failures.push(format!(
                    "{name} [{}] {} {}: status java {} / oxide {}\n--- java\n{}\n--- oxide\n{}",
                    i + 1,
                    w.method,
                    w.path,
                    w.status,
                    g.status,
                    want[i],
                    got[i]
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} steps differ from Java:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
