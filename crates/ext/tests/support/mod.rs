//! Shared fixtures: the shipped example extension, copied into a temp dir so a test can edit its
//! manifest, and placeholders for a temp workdir.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use ext::Placeholders;
use kernel::capability::Capability;

/// `examples/text-stats` in the repository.
pub fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/text-stats")
        .canonicalize()
        .unwrap()
}

/// A temp dir holding `work/` (the session workdir), `home/`, and `ext/` (a copy of the example).
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub workdir: PathBuf,
    pub home: PathBuf,
    pub ext: PathBuf,
}

impl Fixture {
    pub fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workdir = root.join("work");
        let home = root.join("home");
        let ext = root.join("ext");
        for d in [&workdir, &home, &ext.join("schemas")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let src = example_dir();
        for f in ["extension.toml", "server.py", "schemas/top_words.json"] {
            std::fs::copy(src.join(f), ext.join(f)).unwrap();
        }
        Fixture {
            dir,
            workdir,
            home,
            ext,
        }
    }

    pub fn placeholders(&self) -> Placeholders<'_> {
        Placeholders {
            workdir: &self.workdir,
            home: &self.home,
        }
    }

    /// Rewrite the copied manifest with `edit` applied to its text.
    pub fn edit_manifest(&self, edit: impl FnOnce(String) -> String) {
        let path = self.ext.join("extension.toml");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, edit(text)).unwrap();
    }

    /// Replace the copied manifest wholesale.
    pub fn write_manifest(&self, text: &str) {
        std::fs::write(self.ext.join("extension.toml"), text).unwrap();
    }

    /// Grants that cover the example: read the workdir, read the extension's own directory (the
    /// implicit requirement), run python3.
    pub fn grants(&self) -> Vec<Capability> {
        caps(&[
            &format!("fs.rw:{}", self.workdir.display()),
            &format!("fs.ro:{}", self.ext.display()),
            "proc:python3",
        ])
    }
}

pub fn caps(atoms: &[&str]) -> Vec<Capability> {
    atoms.iter().map(|a| a.parse().unwrap()).collect()
}
