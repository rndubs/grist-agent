# `text-stats`: an example out-of-process extension

A third-party tool in the shape ADR-0001 chose: a directory with a manifest and a script. There is
no Rust and no build step. The harness runs it under the sandbox launcher and talks to it over
newline-delimited JSON-RPC on stdio (`docs/specs/extension-manifest.md`).

| File | What |
|---|---|
| `extension.toml` | The manifest: name, version, kind (`stateless`), command, required capabilities (`fs.ro:${workdir}`, `proc:python3`), and the two tools with their input schemas. |
| `schemas/top_words.json` | A tool schema kept in its own file (`schema_file`). |
| `server.py` | The process: standard-library Python answering `initialize`, `tools/list` and `tools/call`. |

The model sees two tools:

- `ext.text_stats.count {path}` → `{lines, words, chars}`
- `ext.text_stats.top_words {path, limit?}` → `{words: [{word, count}]}`

## Loading it

List the directory in an agent profile or a project override:

```toml
# <workdir>/.grist/agent.toml
schema_version = 1

[extensions]
paths = ["${workdir}/.grist/extensions/text-stats"]
```

The profile's grants must cover what the manifest requires, plus read access to the extension's
own directory, which the loader adds implicitly. The shipped `default` agent grants
`fs.rw:${workdir}` and `proc:python3`. That covers `fs.ro:${workdir}`, `proc:python3`, and the
directory itself when it lives under the workdir, as above. A profile without them cannot load
the extension: the launch fails and names the missing atom.

## Trying it by hand

```sh
cd examples/text-stats
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"count","arguments":{"path":"server.py"}}}' \
  | python3 server.py
```

## Making it a session tool

Set `kind = "session"`. The harness then keeps one process for the whole session and sends
every call to it. The script needs no change, because it reads requests until stdin closes.

Tested by `crates/ext/tests/` and `crates/orchestrator/tests/extensions.rs`.
