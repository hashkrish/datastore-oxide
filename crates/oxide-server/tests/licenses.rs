//! The third-party licenses the `license` action prints must cover every crate compiled into the
//! binary: regenerate `src/third_party_licenses.txt` with `tools/gen-licenses.py` after changing
//! dependencies.

use std::process::Command;

#[test]
fn third_party_licenses_are_up_to_date() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let tree = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "oxide-server",
            "-e",
            "normal",
            "--target",
            "all",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        tree.status.success(),
        "{}",
        String::from_utf8_lossy(&tree.stderr)
    );
    let listed = include_str!("../src/third_party_licenses.txt");
    let mut missing = Vec::new();
    for line in String::from_utf8(tree.stdout).unwrap().lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Workspace crates show their path: `oxide-api v0.1.0 (/path)`.
        if parts.len() < 2 || parts.last().unwrap().starts_with('(') {
            continue;
        }
        let entry = format!("{} {} (", parts[0], parts[1].trim_start_matches('v'));
        if !listed.contains(&entry) {
            missing.push(entry);
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "run tools/gen-licenses.py; not listed: {missing:?}"
    );
}
