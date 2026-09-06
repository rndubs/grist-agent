# P0.2 provider spike (throwaway)

One OpenAI-compatible `/v1/chat/completions` client driven by per-endpoint
quirk flags, a fake upstream emulating four streaming shapes, and a runner.
Write-up and quirk table: `docs/spikes/providers.md`. Decision: ADR-0003.

```
cargo test                      # client vs. fake, all shapes, stream + non-stream
./run-against.sh fake           # same via the CLI, prints quirk rows
./run-against.sh standin        # CI stand-in stack (STANDIN_* env)
./run-against.sh vllm|litellm   # humans (GRIST_* env)
cargo run -- fake --port 8089   # keep the fake up for manual poking
cargo run -- --preset vllm --base-url http://host:8000/v1 --model M probe
```

Standalone Cargo package (own `[workspace]`), not part of the root workspace.
Not to be imported by any crate; P1.5 rewrites this against the P1.0 specs.
