//! Admission (`docs/specs/extension-manifest.md` §4): an extension loads only when the profile's
//! grants cover every capability its manifest requires.

mod support;

use ext::{ExtError, Manifest, admit, load_all};
use kernel::sandbox::SandboxLimits;
use kernel::tool::Tool;
use support::{Fixture, caps};

#[test]
fn covered_requirements_are_admitted_with_namespaced_tools() {
    let fx = Fixture::new();
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let tools = admit(&m, &fx.grants(), &SandboxLimits::default()).unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name().to_owned()).collect();
    assert_eq!(names, ["ext.text_stats.count", "ext.text_stats.top_words"]);
    for t in &tools {
        assert_eq!(t.capabilities(), m.capabilities);
        assert!(t.session_command().is_none(), "stateless");
    }
}

#[test]
fn a_profile_cannot_load_an_extension_beyond_its_grants() {
    let fx = Fixture::new();
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let limits = SandboxLimits::default();
    let workdir = fx.workdir.display().to_string();
    let own = format!("fs.ro:{}", fx.ext.display());

    // No grant for the extension's own directory (the implicit requirement).
    let no_own = caps(&[&format!("fs.rw:{workdir}"), "proc:python3"]);
    assert!(matches!(
        admit(&m, &no_own, &limits),
        Err(ExtError::ExceedsGrants { cap, .. }) if cap == own
    ));
    // No grant for the program.
    let no_proc = caps(&[&format!("fs.rw:{workdir}"), &own]);
    assert!(matches!(
        admit(&m, &no_proc, &limits),
        Err(ExtError::ExceedsGrants { extension, cap }) if extension == "text_stats" && cap == "proc:python3"
    ));
    // No grant for the workdir.
    let no_fs = caps(&[&own, "proc:python3"]);
    assert!(matches!(
        admit(&m, &no_fs, &limits),
        Err(ExtError::ExceedsGrants { cap, .. }) if cap == format!("fs.ro:{workdir}")
    ));
    // A grant on a subdirectory does not cover the whole workdir.
    let narrower = caps(&[&format!("fs.rw:{workdir}/src"), &own, "proc:python3"]);
    assert!(matches!(
        admit(&m, &narrower, &limits),
        Err(ExtError::ExceedsGrants { .. })
    ));

    // Read-write is not covered by read-only.
    fx.edit_manifest(|t| t.replace("fs.ro:${workdir}", "fs.rw:${workdir}"));
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let ro_only = caps(&[&format!("fs.ro:{workdir}"), &own, "proc:python3"]);
    assert!(matches!(
        admit(&m, &ro_only, &limits),
        Err(ExtError::ExceedsGrants { cap, .. }) if cap == format!("fs.rw:{workdir}")
    ));

    // Network: a host not in the granted list.
    fx.edit_manifest(|t| t.replace("\"proc:python3\"]", "\"proc:python3\", \"net:pypi.org\"]"));
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let mut grants = fx.grants();
    grants.push("net:example.org".parse().unwrap());
    assert!(matches!(
        admit(&m, &grants, &limits),
        Err(ExtError::ExceedsGrants { cap, .. }) if cap == "net:pypi.org"
    ));
    grants.push("net:pypi.org".parse().unwrap());
    admit(&m, &grants, &limits).unwrap();
}

#[test]
fn load_all_admits_every_directory_or_none() {
    let fx = Fixture::new();
    let limits = SandboxLimits::default();
    let loaded = load_all(
        std::slice::from_ref(&fx.ext),
        &fx.placeholders(),
        &fx.grants(),
        &limits,
    )
    .unwrap();
    assert_eq!(loaded.manifests.len(), 1);
    assert_eq!(loaded.kernel_tools().len(), 2);
    let atoms: Vec<_> = loaded.tool_atoms().iter().map(|c| c.to_string()).collect();
    assert_eq!(
        atoms,
        ["tool:ext.text_stats.count", "tool:ext.text_stats.top_words"]
    );

    // The same extension twice (a copy under another directory) is refused.
    let copy = fx.dir.path().join("copy");
    std::fs::create_dir_all(copy.join("schemas")).unwrap();
    for f in ["extension.toml", "server.py", "schemas/top_words.json"] {
        std::fs::copy(fx.ext.join(f), copy.join(f)).unwrap();
    }
    assert!(matches!(
        load_all(&[fx.ext.clone(), copy], &fx.placeholders(), &fx.grants(), &limits),
        Err(ExtError::DuplicateManifest { name, .. }) if name == "text_stats"
    ));

    // One extension over the grants fails the whole load.
    assert!(matches!(
        load_all(
            std::slice::from_ref(&fx.ext),
            &fx.placeholders(),
            &caps(&["proc:python3"]),
            &limits
        ),
        Err(ExtError::ExceedsGrants { .. })
    ));
}
