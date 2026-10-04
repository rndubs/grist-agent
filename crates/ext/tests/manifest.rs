//! Manifest parsing and validation (`docs/specs/extension-manifest.md` §2).

mod support;

use ext::{ExtError, Manifest};
use kernel::tool::ToolKind;
use serde_json::json;
use support::{Fixture, caps, example_dir};

#[test]
fn the_shipped_example_parses() {
    let fx = Fixture::new();
    let m = Manifest::load(&example_dir(), &fx.placeholders()).unwrap();
    assert_eq!(m.name, "text_stats");
    assert_eq!(m.version, "0.1.0");
    assert_eq!(m.kind, ToolKind::Stateless);
    assert_eq!(m.dir, example_dir());
    assert_eq!(
        m.command,
        [
            "python3".to_owned(),
            example_dir().join("server.py").display().to_string()
        ]
    );
    assert_eq!(m.cwd, fx.workdir, "cwd defaults to ${{workdir}}");
    // The declared atoms plus the implicit read of the extension's own directory, sorted.
    let mut want = caps(&[
        &format!("fs.ro:{}", fx.workdir.display()),
        &format!("fs.ro:{}", example_dir().display()),
        "proc:python3",
    ]);
    want.sort_by_key(|c| c.to_string());
    assert_eq!(m.capabilities, want);
    let names: Vec<_> = m.tools.iter().map(|t| m.tool_name(&t.name)).collect();
    assert_eq!(names, ["ext.text_stats.count", "ext.text_stats.top_words"]);
    // Inline schema and schema file both arrive as JSON objects.
    assert_eq!(m.tools[0].input_schema["required"], json!(["path"]));
    assert_eq!(
        m.tools[1].input_schema["properties"]["limit"]["maximum"],
        json!(100)
    );
    // The hash is of the file bytes.
    let bytes = std::fs::read(example_dir().join("extension.toml")).unwrap();
    assert_eq!(m.hash, kernel::hash::Hash::of_bytes(&bytes));
}

/// Load the fixture after `edit`, expecting an `Invalid` error at `key`.
fn rejects(edit: impl FnOnce(String) -> String, key: &str) -> String {
    let fx = Fixture::new();
    fx.edit_manifest(edit);
    match Manifest::load(&fx.ext, &fx.placeholders()) {
        Err(ExtError::Invalid {
            key: k, message, ..
        }) => {
            assert_eq!(k, key, "{message}");
            message
        }
        other => panic!("expected Invalid at {key}, got {other:?}"),
    }
}

#[test]
fn unknown_keys_are_rejected() {
    rejects(
        |t| t.replace("[extension]\n", "[extension]\ncolour = \"blue\"\n"),
        "extension.colour",
    );
    rejects(|t| format!("dependencies = []\n{t}"), "dependencies");
    rejects(
        |t| t.replace("name = \"count\"\n", "name = \"count\"\nhidden = true\n"),
        "tools[0].hidden",
    );
}

#[test]
fn the_command_program_must_be_declared() {
    let m = rejects(
        |t| t.replace("\"proc:python3\"", "\"proc:bash\""),
        "extension.capabilities",
    );
    assert!(m.contains("proc:python3"), "{m}");
}

#[test]
fn only_sandbox_atoms_may_be_required() {
    for atom in ["tool:read", "spawn:debugger", "secret:API_KEY"] {
        rejects(
            |t| {
                t.replace(
                    "\"proc:python3\"]",
                    &format!("\"proc:python3\", \"{atom}\"]"),
                )
            },
            "extension.capabilities[2]",
        );
    }
    rejects(
        |t| t.replace("\"proc:python3\"]", "\"proc:python3\", \"solver\"]"),
        "extension.capabilities[2]",
    );
}

#[test]
fn placeholders_are_leading_only_and_paths_absolute() {
    rejects(
        |t| t.replace("\"fs.ro:${workdir}\"", "\"fs.ro:/x/${workdir}\""),
        "extension.capabilities[0]",
    );
    rejects(
        |t| t.replace("\"fs.ro:${workdir}\"", "\"fs.ro:${install:x}\""),
        "extension.capabilities[0]",
    );
    rejects(
        |t| t.replace("\"fs.ro:${workdir}\"", "\"fs.ro:${workdir}/../etc\""),
        "extension.capabilities[0]",
    );
    rejects(
        |t| {
            t.replace(
                "kind = \"stateless\"",
                "kind = \"stateless\"\ncwd = \"rel\"",
            )
        },
        "extension.cwd",
    );
}

#[test]
fn kinds_runtimes_and_names() {
    rejects(
        |t| t.replace("kind = \"stateless\"", "kind = \"daemon\""),
        "extension.kind",
    );
    let m = rejects(
        |t| {
            t.replace(
                "kind = \"stateless\"",
                "kind = \"stateless\"\nruntime = \"wasm\"",
            )
        },
        "extension.runtime",
    );
    assert!(m.contains("ADR-0001"), "{m}");
    rejects(
        |t| t.replace("name = \"text_stats\"", "name = \"Text\""),
        "extension.name",
    );
    rejects(
        |t| t.replace("name = \"count\"", "name = \"top_words\""),
        "tools[1].name",
    );
    rejects(
        |t| {
            t.replace(
                "name = \"count\"",
                &format!("name = \"{}\"", "a".repeat(60)),
            )
        },
        "tools[0].name",
    );
    rejects(
        |t| t.replace("schema_version = 1", "schema_version = 2"),
        "schema_version",
    );
}

#[test]
fn tools_need_one_object_schema() {
    rejects(
        |t| t.replace("schema_file = \"schemas/top_words.json\"", ""),
        "tools[1].input_schema",
    );
    rejects(
        |t| {
            t.replace(
                "type = \"object\", properties",
                "type = \"array\", properties",
            )
        },
        "tools[0].input_schema",
    );
    rejects(
        |t| t.replace("schemas/top_words.json", "../escape.json"),
        "tools[1].schema_file",
    );
    rejects(
        |t| t.replace("schemas/top_words.json", "/etc/passwd"),
        "tools[1].schema_file",
    );
    let fx = Fixture::new();
    fx.write_manifest(
        "schema_version = 1\n[extension]\nname = \"x\"\nversion = \"1\"\nkind = \"session\"\n\
         command = [\"python3\"]\ncapabilities = [\"proc:python3\"]\n",
    );
    assert!(matches!(
        Manifest::load(&fx.ext, &fx.placeholders()),
        Err(ExtError::Invalid { key, .. }) if key == "tools"
    ));
}

#[test]
fn a_symlinked_schema_file_cannot_leave_the_directory() {
    let fx = Fixture::new();
    let outside = fx.dir.path().join("outside.json");
    std::fs::write(&outside, r#"{"type":"object"}"#).unwrap();
    std::fs::remove_file(fx.ext.join("schemas/top_words.json")).unwrap();
    std::os::unix::fs::symlink(&outside, fx.ext.join("schemas/top_words.json")).unwrap();
    assert!(matches!(
        Manifest::load(&fx.ext, &fx.placeholders()),
        Err(ExtError::Invalid { key, .. }) if key == "tools[1].schema_file"
    ));
}

#[test]
fn missing_and_malformed_files() {
    let fx = Fixture::new();
    assert!(matches!(
        Manifest::load(&fx.dir.path().join("nope"), &fx.placeholders()),
        Err(ExtError::Io { .. })
    ));
    fx.write_manifest("schema_version = [");
    assert!(matches!(
        Manifest::load(&fx.ext, &fx.placeholders()),
        Err(ExtError::Parse { .. })
    ));
}
