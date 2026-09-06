//! CLI for the P0.2 provider spike.
//!
//!   provider-spike --preset vllm --base-url http://host:8000/v1 --model Qwen/Qwen3-8B probe
//!   provider-spike --config configs/litellm.toml tools
//!   provider-spike fake --port 8089

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Deserialize;

use provider_spike::client::{Client, Endpoint};
use provider_spike::probe;
use provider_spike::quirks::{Auth, Quirks};

#[derive(Parser, Debug)]
#[command(
    name = "provider-spike",
    about = "P0.2 provider spike: one OpenAI-compatible client with quirk flags"
)]
struct Cli {
    /// TOML config: base_url, model, api_key_env, preset, [quirks] overrides.
    #[arg(long, global = true)]
    config: Option<String>,
    /// Overrides config. e.g. http://127.0.0.1:8000/v1
    #[arg(long, global = true)]
    base_url: Option<String>,
    #[arg(long, global = true)]
    model: Option<String>,
    /// Name of the env var holding the API key (the value is read at request time and never printed).
    #[arg(long, global = true)]
    api_key_env: Option<String>,
    /// Quirk preset: vllm | litellm | llamacpp | hermes | probe
    #[arg(long, global = true)]
    preset: Option<String>,
    /// Print request bodies (never headers) to stderr.
    #[arg(long, global = true)]
    dump_requests: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Plain completion (streaming by default).
    Complete {
        #[arg(long)]
        no_stream: bool,
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Two-turn tool-call loop with the get_weather tool.
    Tools {
        #[arg(long)]
        no_stream: bool,
    },
    /// Reasoning capture.
    Reasoning {
        #[arg(long)]
        no_stream: bool,
    },
    /// Structured-output probe (response_format json_schema).
    Structured,
    /// Run every scenario and print the observed quirk row.
    Probe {
        /// Row label in the markdown output.
        #[arg(long, default_value = "endpoint")]
        label: String,
        /// Write the JSON report here as well.
        #[arg(long)]
        out: Option<String>,
        /// Comma-separated scenarios that are informational only (do not affect the
        /// exit code). Default: structured. Use `structured,reasoning` for a
        /// non-reasoning stand-in model.
        #[arg(long, default_value = "structured")]
        optional: String,
    },
    /// Serve the fake upstream. Shapes at /{vllm,litellm,llamacpp,hermes}/v1.
    Fake {
        #[arg(long, default_value_t = 8089)]
        port: u16,
        /// Bytes per streamed body chunk (small = exercises partial-line parsing).
        #[arg(long, default_value_t = 41)]
        chunk_bytes: usize,
    },
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    base_url: Option<String>,
    model: Option<String>,
    api_key_env: Option<String>,
    preset: Option<String>,
    quirks: Option<toml::Table>,
}

fn resolve_endpoint(cli: &Cli) -> Result<Endpoint, String> {
    let file: FileConfig = match &cli.config {
        Some(p) => {
            let s = std::fs::read_to_string(p).map_err(|e| format!("read {p}: {e}"))?;
            toml::from_str(&s).map_err(|e| format!("parse {p}: {e}"))?
        }
        None => FileConfig::default(),
    };
    let preset = cli
        .preset
        .clone()
        .or(file.preset.clone())
        .unwrap_or_else(|| "probe".into());
    let mut quirks = Quirks::preset(&preset).ok_or_else(|| format!("unknown preset {preset}"))?;
    if let Some(over) = file.quirks {
        // Merge: serialize the preset, overlay the table, deserialize back.
        let mut base: toml::Table = toml::Table::try_from(&quirks).map_err(|e| e.to_string())?;
        for (k, v) in over {
            base.insert(k, v);
        }
        quirks = base.try_into().map_err(|e| format!("quirks: {e}"))?;
    }
    if let Some(env) = cli.api_key_env.clone().or(file.api_key_env) {
        quirks.auth = Auth::Bearer { env };
    }
    let base_url = cli
        .base_url
        .clone()
        .or(file.base_url)
        .ok_or("base_url required (--base-url or config)")?;
    let model = cli
        .model
        .clone()
        .or(file.model)
        .ok_or("model required (--model or config)")?;
    Ok(Endpoint {
        base_url,
        model,
        quirks,
    })
}

fn print_completion(c: &provider_spike::types::Completion, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(c).unwrap());
        return;
    }
    if let Some(t) = &c.response.thinking {
        println!("[thinking] {t}");
    }
    println!("[text] {}", c.response.text);
    for tc in &c.response.tool_calls {
        println!(
            "[tool_call] id={} name={} arguments={}",
            tc.id, tc.name, tc.arguments
        );
    }
    println!("[usage] {:?}", c.response.usage);
    println!(
        "[observed] finish_reason={:?} chunks={} reasoning_fields_seen={:?} usage_in_final_chunk={} tool_call_delta_chunks={} parsed_from_text={}",
        c.observed.finish_reason,
        c.observed.chunks,
        c.observed.reasoning_fields_seen,
        c.observed.usage_in_final_chunk,
        c.observed.tool_call_delta_chunks,
        c.observed.tool_calls_parsed_from_text
    );
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Cmd::Fake { port, chunk_bytes } = &cli.cmd {
        let (addr, handle) = match provider_spike::fake::serve(*port, *chunk_bytes).await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("bind: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!(
            "fake upstream listening on http://{addr}  shapes: /vllm/v1 /litellm/v1 /llamacpp/v1 /hermes/v1"
        );
        let _ = handle.await;
        return ExitCode::SUCCESS;
    }

    let endpoint = match resolve_endpoint(&cli) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("config: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut client = match Client::new(endpoint) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client: {e}");
            return ExitCode::FAILURE;
        }
    };
    client.dump_requests = cli.dump_requests;

    let result: Result<bool, provider_spike::client::Error> = match &cli.cmd {
        Cmd::Complete { no_stream, prompt } => {
            let mut req = probe::plain_request(!no_stream);
            if let Some(p) = prompt {
                req.messages = vec![provider_spike::types::user(p)];
            }
            client.complete(&req).await.map(|c| {
                print_completion(&c, cli.json);
                !c.response.text.is_empty()
            })
        }
        Cmd::Reasoning { no_stream } => client
            .complete(&probe::reasoning_request(!no_stream))
            .await
            .map(|c| {
                print_completion(&c, cli.json);
                c.response.thinking.is_some()
            }),
        Cmd::Tools { no_stream } => {
            probe::tool_loop(&client, !no_stream)
                .await
                .map(|(first, second)| {
                    print_completion(&first, cli.json);
                    if let Some(s) = &second {
                        println!("--- after tool result ---");
                        print_completion(s, cli.json);
                    }
                    !first.response.tool_calls.is_empty()
                        && second.is_some_and(|s| !s.response.text.is_empty())
                })
        }
        Cmd::Structured => probe::structured_probe(&client).await.map(|o| {
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&o).unwrap());
            } else {
                println!("{o:?}");
            }
            matches!(o, probe::StructuredOutcome::Honored { .. })
        }),
        Cmd::Probe {
            label,
            out,
            optional,
        } => {
            let report = probe::run_all(&client).await;
            let optional: Vec<&str> = optional
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!("{}", report.markdown(label));
            }
            if let Some(p) = out
                && let Err(e) = std::fs::write(p, serde_json::to_string_pretty(&report).unwrap())
            {
                eprintln!("write {p}: {e}");
            }
            let required_ok = report
                .scenarios
                .iter()
                .filter(|s| !optional.contains(&s.name.as_str()))
                .all(|s| s.ok);
            if !required_ok {
                eprintln!("required scenarios failed (optional: {optional:?})");
            }
            Ok(required_ok)
        }
        Cmd::Fake { .. } => unreachable!(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("scenario did not meet its success condition");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
