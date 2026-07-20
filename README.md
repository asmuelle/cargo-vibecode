# cargo-vibecode

[![CI](https://github.com/asmuelle/cargo-vibecode/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/asmuelle/cargo-vibecode/actions/workflows/ci.yml)
[![Release](https://github.com/asmuelle/cargo-vibecode/actions/workflows/release.yml/badge.svg)](https://github.com/asmuelle/cargo-vibecode/actions/workflows/release.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust edition](https://img.shields.io/badge/rust-2024%20%7C%201.95%2B-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/index.html)

Repository for the `cargo-vibe` CLI orchestrator. The installed Cargo subcommand remains `cargo vibe`.

```
cargo vibe check ──► runs diff-risk + cargo-impact + spec-drift, aggregates results
cargo vibe fix   ──► context → LLM → risk veto → impact verify → drift check → retry
cargo vibe risk  ──► semantic risk scoring (delegates to diff-risk)
cargo vibe impact──► blast-radius analysis (delegates to cargo-impact)
cargo vibe drift ──► spec coherence check (delegates to spec-drift)
cargo vibe context──► token-budgeted context assembly (delegates to cargo-context)
```

## The Problem

AI-assisted Rust development has four failure modes:

1. **LLM generates dangerous code** — `unsafe` blocks, auth bypasses, serde wire breaks
2. **LLM doesn't know what to test** — runs all 5,000 tests instead of 12
3. **Documentation rots silently** — README says `fn old()` but code has `fn new()`
4. **Context quality determines output quality** — dumping all files wastes tokens and attention

Four tools solve these individually. `cargo-vibecode` packages the `cargo-vibe` orchestrator that runs them as a single workflow.

## Installation

From a released tag:

```bash
cargo install --git https://github.com/asmuelle/cargo-vibecode --tag v0.2.0 cargo-vibe
```

From a local checkout:

```bash
cargo install --path .
```

For full coverage, install the individual tools:

```bash
cargo install cargo-context
cargo install diff-risk
cargo install cargo-impact
cargo install spec-drift
```

`cargo vibe check` degrades gracefully when one of the companion tools is absent: it prints a warning and skips that tool. `cargo vibe fix` fails closed if `diff-risk` is unavailable, because the risk veto is the core safety gate for that workflow.

## Commands

### `cargo vibe check`

Run all health checks against the current diff:

```bash
cargo vibe check --since HEAD --strict
```

Output:
```
cargo-vibe: running health check against HEAD...
  [1/3] diff-risk... passed
  [2/3] cargo-impact... 3 finding(s)
    🔴 test_reference tests/auth.rs:42  login() test references changed symbol
    🟡 trait_impl src/handlers.rs:15  impl AuthHandler for App
    🟡 doc_drift_link README.md:23  [login] references changed symbol
  [3/3] spec-drift... 1 divergence(s)
    ❌ symbol_absence README.md:10  `connect_db()` not found in code

  Total: 4 finding(s) | Critical: 0 | High: 1
```

Format options:
```bash
cargo vibe check --format json     # Machine-readable
cargo vibe check --format sarif    # GitHub Code Scanning
```

### `cargo vibe fix`

The AI fix loop:

```
┌──────────┐    ┌──────────┐    ┌──────────┐    ┌──────────┐
│ Context  │───►│ LLM      │───►│ Risk     │───►│ Verify   │
│ Assembly │    │ Generate │    │ Check    │    │ Impact   │
└──────────┘    └──────────┘    └──────────┘    └──────────┘
                                                     │
                                              ┌──────▼──────┐
                                              │ Drift Check │
                                              └──────┬──────┘
                                                     │
                                              ┌──────▼──────┐
                                              │ Pass?  Yes──► Ship
                                              │ No ────────► Retry
                                              └─────────────┘
```

Usage:
```bash
cargo vibe fix --prompt "Implement user authentication with JWT tokens"
```

The loop:
1. Assembles context pack (compiler errors, diff, project map, entry points, related tests)
2. Presents context + prompt for the user to paste into their LLM
3. *Optional, opt-in via `--auto-heal`*: runs the Clippy Auto-Healer — up to 5 passes of `cargo clippy --all-targets`, auto-applying only `MachineApplicable` suggestions. It prints a per-file summary of every applied fix (count and lint names) before the loop continues, and the auto-applied changes are included in the diff scored by the next step. Off by default; when the flag is absent no clippy run happens and no files are touched.
4. After user applies changes: runs `diff-risk` — rejects if score ≥ threshold (default 7.0)
5. Runs `cargo-impact --fail-on high` — surfaces affected tests
6. Runs `spec-drift --deny warning` — checks for doc/CI drift
7. If all pass: done. If any fail: feeds failures back for retry (up to `--max-attempts`, default 3)

The generation step is automated in `v0.2.0` via standard programmatic client integrations (`LlmClient`), defaulting to Ollama or OpenAI/vLLM for local-first privacy while prompting with interactive unified diffs for confirmation.

Options:
```bash
cargo vibe fix \
  --prompt "Refactor the auth module" \
  --max-attempts 5 \
  --risk-threshold 6.0 \
  --since origin/main \
  --auto-heal          # opt in to clippy auto-healing (default: off)
```

### `cargo vibe risk`

Semantic risk scoring (wraps `diff-risk`):

```bash
cargo vibe risk --since HEAD --threshold 7.0
```

### `cargo vibe impact`

Blast-radius analysis (wraps `cargo-impact`):

```bash
cargo vibe impact --since HEAD --format json
cargo vibe impact --test           # cargo-nextest filter expression
```

### `cargo vibe drift`

Spec coherence check (wraps `spec-drift`):

```bash
cargo vibe drift --diff HEAD --format sarif
cargo vibe drift --deny critical   # Only fail on critical divergences
```

### `cargo vibe context`

Token-budgeted context assembly (wraps `cargo-context`):

```bash
cargo vibe context --preset fix --budget 32000
echo "why does this test fail?" | cargo vibe context --stdin
```

## Configuration

All tools read `.cargo-vibe.toml` from the project root:

```toml
[vibe]
# Global settings applied to all tools
threshold = 7.0             # Default risk threshold
token_budget = 32000        # Default context budget
tokenizer = "cl100k"        # gpt-4, claude
scrub = true                # Enable secret scrubbing

[diff_risk]
enabled = true
threshold = 8.0             # Tool-specific override
extra_args = ["--strict"]

[cargo_impact]
enabled = true
extra_args = ["--confidence-min", "0.7"]

[spec_drift]
enabled = true

[cargo_context]
enabled = true
```

Tool-specific config files (`.cargo-context/config.yaml`, `cargo-impact.toml`, `spec-drift.toml`) continue to work independently.

You can also pass an explicit config path:

```bash
cargo vibe --config ./ci/cargo-vibe.toml check --strict --since origin/main
```

The top-level config controls orchestration behavior: `enabled = false` skips a tool, `extra_args` are appended to the subprocess invocation, `[diff_risk].threshold` overrides `[vibe].threshold`, and `[vibe].token_budget` is used by `cargo vibe context` when `--budget` is omitted.

## CI Integration

### GitHub Actions

```yaml
- name: Vibe Check
  run: |
    cargo vibe check --since origin/main --strict --format sarif
  continue-on-error: true

- name: Upload SARIF
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: vibe-results.sarif
```

For the `cargo-vibe` repository itself, the internal git-tag release gate is:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo install --path . --locked
```

This `v0.2.0` release resolves `ai-tools-core` from the `asmuelle/ai-tools-core` `v0.2.0` git tag. A crates.io release additionally requires publishing `ai-tools-core` to crates.io and changing the dependency to the registry version before running `cargo package` / `cargo publish --dry-run`.

### Pre-commit Hook

```bash
#!/bin/sh
# .git/hooks/pre-commit
cargo vibe check --since HEAD --strict || exit 1
```

## Architecture

```
cargo-vibecode
└── cargo-vibe (orchestrator)
    ├── ai-tools-core (shared types)
    │     ├── finding    — Finding, Severity, Confidence, Location
    │     ├── sarif      — SARIF v2.1.0 renderer
    │     ├── scrub      — Secret scrubbing pipeline
    │     ├── git_utils  — diff, blame, changed files
    │     ├── cargo_utils— metadata, workspace discovery
    │     └── config     — .cargo-vibe.toml parser
    │
    ├── diff-risk ─────── risk scoring (regex detectors + custom DSL)
    ├── cargo-impact ──── blast-radius analysis (syn + rust-analyzer)
    ├── spec-drift ────── spec coherence (docs/examples/tests/CI)
    └── cargo-context ─── context assembly (token-budgeted packs)
```

Each tool is independently usable. `cargo-vibecode` adds orchestration, feedback loops, and unified reporting through the `cargo-vibe` CLI.

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All checks passed (or no blocking findings) |
| 1 | Threshold exceeded — findings exist at or above the configured severity |
| 2 | Tool error — config parse failure, missing dependency, I/O error |

Note: `cargo vibe check` treats missing companion tools as skipped checks, not tool errors. Wrapper commands such as `cargo vibe risk`, `cargo vibe impact`, `cargo vibe drift`, and `cargo vibe context` return code 2 when their required subprocess cannot be started. `cargo vibe fix` also returns code 2 when `diff-risk` is unavailable.

## Dependencies

| Crate | Why |
|-------|-----|
| `clap` | CLI argument parsing with subcommands |
| `anyhow` | Error handling |
| `serde` / `serde_json` | SARIF and JSON output |
| `ai-tools-core` | Shared types, utils, and config |

## License

MIT OR Apache-2.0
