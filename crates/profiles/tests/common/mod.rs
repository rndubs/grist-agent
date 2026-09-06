//! Shared fixture: a temporary `profiles/` tree, a workdir, a home, and the P1.7 registry.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kernel::{Capability, ToolKind};
use profiles::{Diagnostic, Registry, ResolveInputs, Resolved, ToolDecl, resolve};
use tempfile::TempDir;

/// The §5.2 bundles file.
pub const BUNDLES: &str = include_str!("../../../../profiles/bundles.toml");
/// The §2.8 stand-in model profile.
pub const STAND_IN: &str = include_str!("../../../../profiles/models/stand-in.toml");

/// A minimal valid default agent.
pub const MIN_AGENT: &str = r#"schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
"#;

/// A catalog with only `default`.
pub const CATALOG: &str = r#"schema_version = 1
[[agents]]
name = "default"
model_profile = "models/stand-in.toml"
agent_profile = "agents/default.toml"
"#;

pub struct Tree {
    pub root: TempDir,
    pub profiles: PathBuf,
    pub workdir: PathBuf,
    pub home: PathBuf,
}

impl Tree {
    /// A tree with the shipped bundles, the stand-in model, a minimal default agent, and a
    /// one-entry catalog. Tests overwrite whichever file the case is about.
    pub fn new() -> Tree {
        let root = TempDir::new().unwrap();
        let profiles = root.path().join("profiles");
        let workdir = root.path().join("work");
        let home = root.path().join("home");
        std::fs::create_dir_all(profiles.join("models")).unwrap();
        std::fs::create_dir_all(profiles.join("agents")).unwrap();
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let t = Tree {
            root,
            profiles,
            workdir,
            home,
        };
        t.write("bundles.toml", BUNDLES);
        t.write("models/stand-in.toml", STAND_IN);
        t.write("agents/default.toml", MIN_AGENT);
        t.write("catalog.toml", CATALOG);
        t
    }

    /// Write a file under `profiles/`.
    pub fn write(&self, rel: &str, content: &str) {
        let p = self.profiles.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Write a file under the workdir.
    pub fn write_workdir(&self, rel: &str, content: &str) {
        let p = self.workdir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// Add a catalog entry (models/stand-in.toml + agents/<name>.toml) alongside `default`.
    pub fn catalog_with(&self, extra: &[&str]) {
        let mut s = CATALOG.to_owned();
        for name in extra {
            s.push_str(&format!(
                "[[agents]]\nname = \"{name}\"\nmodel_profile = \"models/stand-in.toml\"\nagent_profile = \"agents/{name}.toml\"\n"
            ));
        }
        self.write("catalog.toml", &s);
    }

    pub fn resolve(&self, agent: &str) -> Result<Resolved, Vec<Diagnostic>> {
        self.resolve_with(
            agent,
            &registry_for(&self.workdir),
            &toml::Table::new(),
            false,
        )
    }

    pub fn resolve_with(
        &self,
        agent: &str,
        registry: &Registry,
        overrides: &toml::Table,
        resume: bool,
    ) -> Result<Resolved, Vec<Diagnostic>> {
        resolve(&ResolveInputs {
            profiles_dir: &self.profiles,
            workdir: &self.workdir,
            home: &self.home,
            agent,
            runtime_overrides: overrides,
            registry,
            resume,
        })
    }
}

fn cap(s: &str) -> Capability {
    s.parse().unwrap()
}

/// The six P1.7 tool declarations with `${workdir}` bound to `workdir`.
pub fn registry_for(workdir: &Path) -> Registry {
    let w = workdir.to_string_lossy();
    let ro = cap(&format!("fs.ro:{w}"));
    let rw = cap(&format!("fs.rw:{w}"));
    let decl = |kind: ToolKind, caps: Vec<Capability>| ToolDecl {
        kind,
        capabilities: caps,
    };
    let tools = BTreeMap::from([
        // D17: the host's question tool needs no atom.
        ("ask_user".to_owned(), decl(ToolKind::Stateless, vec![])),
        (
            "read".to_owned(),
            decl(ToolKind::Stateless, vec![ro.clone()]),
        ),
        (
            "write".to_owned(),
            decl(ToolKind::Stateless, vec![rw.clone()]),
        ),
        (
            "edit".to_owned(),
            decl(ToolKind::Stateless, vec![rw.clone()]),
        ),
        (
            "bash".to_owned(),
            decl(ToolKind::Stateless, vec![rw.clone(), cap("proc:bash")]),
        ),
        (
            "run_script".to_owned(),
            decl(ToolKind::Stateless, vec![rw.clone(), cap("proc:bash")]),
        ),
        (
            "python".to_owned(),
            decl(ToolKind::Session, vec![rw.clone(), cap("proc:python3")]),
        ),
    ]);
    Registry {
        tools,
        middleware: BTreeSet::new(),
        parsers: BTreeSet::new(),
        memory_modules: BTreeSet::from(["none".to_owned()]),
        sandbox_backends: BTreeSet::from(["bwrap".to_owned(), "none".to_owned()]),
        dev_build: false,
    }
}

/// `Display` of the first diagnostic of a failed resolution.
pub fn first(r: Result<Resolved, Vec<Diagnostic>>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(diags) => {
            assert!(!diags.is_empty());
            diags[0].to_string()
        }
    }
}

/// Assert the first diagnostic's `Display` starts with `prefix`.
#[track_caller]
pub fn assert_first(r: Result<Resolved, Vec<Diagnostic>>, prefix: &str) {
    let d = first(r);
    assert!(
        d.starts_with(prefix),
        "expected prefix {prefix:?}\n     got {d:?}"
    );
}

/// Assert the first diagnostic's `Display` equals `exact`.
#[track_caller]
pub fn assert_first_exact(r: Result<Resolved, Vec<Diagnostic>>, exact: &str) {
    let d = first(r);
    assert_eq!(d, exact);
}

/// Warning codes of a successful resolution.
pub fn warning_codes(r: &Resolved) -> Vec<&'static str> {
    r.warnings.iter().map(|w| w.code).collect()
}
