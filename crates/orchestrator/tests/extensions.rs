//! P2.1 end to end: a profile names an out-of-process extension in `[extensions].paths`, the
//! launcher admits it against the profile's grants, and a real kernel calls its tools through the
//! sandbox launchers (`None` backend, D14). The extension is the shipped `examples/text-stats`,
//! copied into the checkout's `.grist/extensions/` the way an agent would author one, and loaded
//! through the agent-writable project override (layer 3).
#![cfg(feature = "dev-sandbox-none")]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kernel::*;
use orchestrator::launcher::{LaunchError, LaunchOptions, create_session};
use serde_json::{Value, json};

struct Scripted(Mutex<VecDeque<ModelResponse>>);

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, _req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted"))
    }
}

fn response(content: Vec<ContentBlock>, stop: StopReason) -> ModelResponse {
    let raw = serde_json::to_vec(&content).unwrap();
    ModelResponse {
        content,
        stop_reason: stop,
        usage: Usage::default(),
        model_id: "scripted".into(),
        raw_response_hash: Hash::of_bytes(&raw),
        response_id: None,
    }
}

fn calls(uses: &[(&str, &str, Value)]) -> ModelResponse {
    response(
        uses.iter()
            .map(|(id, name, input)| ContentBlock::ToolUse {
                id: (*id).into(),
                name: (*name).into(),
                input: input.clone(),
            })
            .collect(),
        StopReason::ToolUse,
    )
}

fn done() -> ModelResponse {
    response(
        vec![ContentBlock::Text {
            text: "Done.".into(),
        }],
        StopReason::EndTurn,
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    ext: PathBuf,
    opts: LaunchOptions,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let ext = repo.join(".grist/extensions/text-stats");
    std::fs::create_dir_all(ext.join("schemas")).unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/text-stats");
    for f in ["extension.toml", "server.py", "schemas/top_words.json"] {
        std::fs::copy(example.join(f), ext.join(f)).unwrap();
    }
    std::fs::write(repo.join("AGENTS.md"), "Be brief.\n").unwrap();
    std::fs::write(repo.join("notes.txt"), "red green\nred blue red\n").unwrap();
    std::fs::write(
        repo.join(".grist/agent.toml"),
        "schema_version = 1\n\n[extensions]\npaths = [\"${workdir}/.grist/extensions/text-stats\"]\n",
    )
    .unwrap();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let opts = LaunchOptions {
        profiles_dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../profiles")
            .canonicalize()
            .unwrap(),
        state_dir: root.join("state"),
        home,
        sandbox_override: Some("none".into()),
        question_capacity: 4,
        delta_capacity: 64,
    };
    Fixture {
        _dir: dir,
        repo,
        ext,
        opts,
    }
}

fn events(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["kind"] == kind).collect()
}

async fn run(fx: &Fixture, script: Vec<ModelResponse>) -> Vec<Value> {
    run_with(&fx.opts, fx, script).await
}

async fn run_with(opts: &LaunchOptions, fx: &Fixture, script: Vec<ModelResponse>) -> Vec<Value> {
    let provider = Arc::new(Scripted(Mutex::new(script.into())));
    let mut launched = create_session(
        opts,
        SessionId("s_ext".into()),
        &fx.repo,
        "default",
        toml::Table::new(),
        Some(provider),
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    launched
        .kernel
        .handle()
        .enqueue_user_message(Message::user_text("How many words are in notes.txt?"))
        .unwrap();
    assert_eq!(launched.kernel.run().await.unwrap(), RunStop::Idle);
    events(&launched.log_path)
}

#[tokio::test]
async fn a_profile_loads_a_stateless_extension_and_the_model_calls_it() {
    let fx = fixture();
    let log = run(
        &fx,
        vec![
            calls(&[
                ("c1", "ext.text_stats.count", json!({"path": "notes.txt"})),
                (
                    "c2",
                    "ext.text_stats.top_words",
                    json!({"path": "notes.txt", "limit": 1}),
                ),
            ]),
            done(),
        ],
    )
    .await;

    let created = &of_kind(&log, "session_created")[0]["payload"];
    let tools: Vec<&str> = created["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(tools.contains(&"ext.text_stats.count"), "{tools:?}");
    assert!(tools.contains(&"ext.text_stats.top_words"), "{tools:?}");
    let grants = created["grants"].to_string();
    assert!(grants.contains("tool:ext.text_stats.count"), "{grants}");

    let results = of_kind(&log, "tool_result");
    assert_eq!(results.len(), 2);
    for r in &results {
        assert_eq!(r["payload"]["is_error"], json!(false), "{r}");
    }
    let content = |i: usize| results[i]["payload"]["content"].to_string();
    assert!(content(0).contains("\"words\":5"), "{}", content(0));
    assert!(content(1).contains("\"word\":\"red\""), "{}", content(1));
}

#[tokio::test]
async fn a_session_extension_serves_calls_over_rpc() {
    let fx = fixture();
    let manifest = fx.ext.join("extension.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace("kind = \"stateless\"", "kind = \"session\""),
    )
    .unwrap();
    let log = run(
        &fx,
        vec![
            calls(&[("c1", "ext.text_stats.count", json!({"path": "notes.txt"}))]),
            calls(&[("c2", "ext.text_stats.count", json!({"path": "missing.txt"}))]),
            done(),
        ],
    )
    .await;
    let results = of_kind(&log, "tool_result");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["payload"]["is_error"], json!(false));
    // A failure in the extension's own code reaches the model as an error result.
    assert_eq!(results[1]["payload"]["is_error"], json!(true));
    assert!(
        results[1]["payload"]["content"]
            .to_string()
            .contains("cannot read missing.txt")
    );
}

#[tokio::test]
async fn a_profile_cannot_load_an_extension_that_exceeds_its_grants() {
    let fx = fixture();
    // The default agent grants `fs.rw:${workdir}`, `proc:bash`, `proc:python3`; ask for the home
    // directory as well.
    let manifest = fx.ext.join("extension.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace(
            "capabilities = [\"fs.ro:${workdir}\", \"proc:python3\"]",
            "capabilities = [\"fs.ro:${workdir}\", \"fs.ro:${home}\", \"proc:python3\"]",
        ),
    )
    .unwrap();
    let err = create_session(
        &fx.opts,
        SessionId("s_ext_refused".into()),
        &fx.repo,
        "default",
        toml::Table::new(),
        Some(Arc::new(Scripted(Mutex::new(VecDeque::new())))),
    )
    .await
    .err()
    .expect("the launch is refused");
    let home = fx.opts.home.display().to_string();
    assert!(
        matches!(&err, LaunchError::Extension(ext::ExtError::ExceedsGrants { extension, cap })
            if extension == "text_stats" && *cap == format!("fs.ro:{home}")),
        "{err}"
    );
}

/// The same stateless call through the profile's own `bwrap` backend (no override). Skips without
/// `bwrap` unless `GRIST_REQUIRE_BWRAP=1` (the CI `test` job).
#[tokio::test]
async fn the_extension_runs_under_bwrap_through_the_launcher() {
    if !::sandbox::bwrap::program_available("bwrap") {
        assert_ne!(
            std::env::var("GRIST_REQUIRE_BWRAP").as_deref(),
            Ok("1"),
            "bwrap not on PATH but GRIST_REQUIRE_BWRAP=1"
        );
        println!("skipping: bwrap not available");
        return;
    }
    let fx = fixture();
    let opts = LaunchOptions {
        sandbox_override: None,
        ..fx.opts.clone()
    };
    let log = run_with(
        &opts,
        &fx,
        vec![
            calls(&[("c1", "ext.text_stats.count", json!({"path": "notes.txt"}))]),
            done(),
        ],
    )
    .await;
    assert_eq!(
        of_kind(&log, "session_created")[0]["payload"]["sandbox_backend"],
        json!("bwrap")
    );
    let results = of_kind(&log, "tool_result");
    assert_eq!(
        results[0]["payload"]["is_error"],
        json!(false),
        "{}",
        results[0]
    );
    assert!(
        results[0]["payload"]["content"]
            .to_string()
            .contains("\"words\":5")
    );
}
