# Spikes (Phase 0)

Throwaway code that de-risks a design assumption. Spike code is **not** a
deliverable; the write-up under `docs/spikes/` and the resulting ADR are.

Rules:

- Each spike lives in its own directory here with its own build (a standalone
  `Cargo.toml`, a `pyproject.toml`, shell scripts, whatever is fastest).
- `spikes/` is excluded from the Cargo workspace and from CI's workspace jobs.
  Nothing in `crates/` may depend on anything here.
- A spike is finished when its write-up in `docs/spikes/<name>.md` records exact
  commands, versions, and the outcome, and the relevant ADR is drafted.
- Do not polish spike code. If a spike's code turns out to be worth keeping, it
  is rewritten in the proper crate against the P1.0 specs, not moved.

| Spike | Milestone | Write-up |
|---|---|---|
| Sandbox nesting on the HPC login node | P0.1 | `docs/spikes/sandbox-nesting.md` |
| Provider client (vLLM + LiteLLM + stand-in) | P0.2 | `docs/spikes/providers.md` |
| Out-of-process tool mechanism | P0.3 | `docs/spikes/extension-mechanism.md` |
