//! Enforces the kernel-boundary rule from `crates/README.md`: `kernel` depends
//! on no other in-repo crate. Any `path = "..."` dependency or a dependency on a
//! sibling workspace crate name fails this test.
//!
//! Cargo itself would also reject most violations as a dependency cycle (every
//! sibling depends on `kernel`), but this test names the rule explicitly and
//! catches the non-cyclic cases (a `path` dependency on something outside the
//! workspace, or a future crate that does not yet depend on `kernel`).

const MANIFEST: &str = include_str!("../Cargo.toml");

/// Every workspace crate other than `kernel` itself.
const SIBLINGS: &[&str] = &[
    "providers",
    "host",
    "ext",
    "profiles",
    "sandbox",
    "orchestrator",
    "provenance",
    "evolve",
];

/// Returns the manifest lines that sit inside any `[dependencies]`-like table
/// (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, and their
/// `[target.'cfg(..)'.*]` variants).
fn dependency_lines(manifest: &str) -> Vec<&str> {
    let mut in_deps = false;
    let mut out = Vec::new();
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_deps = line.trim_matches(['[', ']']).ends_with("dependencies");
            continue;
        }
        if in_deps && !line.is_empty() && !line.starts_with('#') {
            out.push(line);
        }
    }
    out
}

/// Returns the first offending dependency line, if any.
fn violation(manifest: &str) -> Option<String> {
    for line in dependency_lines(manifest) {
        if line.contains("path =") {
            return Some(format!("path dependency: `{line}`"));
        }
        let key = line.split(['=', '.']).next().unwrap_or("").trim();
        if SIBLINGS.contains(&key) {
            return Some(format!("in-repo crate `{key}`: `{line}`"));
        }
    }
    None
}

#[test]
fn kernel_has_no_in_repo_dependencies() {
    if let Some(why) = violation(MANIFEST) {
        panic!("kernel must not depend on any in-repo crate; found {why}");
    }
}

#[test]
fn checker_rejects_sibling_dependency() {
    let bad = "[package]\nname = \"kernel\"\n\n[dependencies]\nhost.workspace = true\n";
    assert!(violation(bad).is_some());
    let bad =
        "[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\nsandbox = { workspace = true }\n";
    assert!(violation(bad).is_some());
}

#[test]
fn checker_rejects_path_dependency() {
    let bad = "[dependencies]\nfoo = { path = \"../../spikes/foo\" }\n";
    assert!(violation(bad).is_some());
}

#[test]
fn checker_accepts_third_party_dependencies() {
    let ok = "[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\ntokio.workspace = true\n\n[lints]\nworkspace = true\n";
    assert!(violation(ok).is_none());
}
