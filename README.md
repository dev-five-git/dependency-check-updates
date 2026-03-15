# dcu

**Dependency Check & Update** — a fast, multi-ecosystem dependency updater written in Rust.

Like [npm-check-updates](https://www.npmjs.com/package/npm-check-updates), but for every language.

```
$ dcu
Checking Cargo.toml
 toml_edit  0.22  ->  0.25.4

Run dcu -u to upgrade Cargo.toml
```

## Features

- **Multi-ecosystem** — `package.json`, `Cargo.toml`, `pyproject.toml` in a single tool
- **Format-preserving** — surgical byte-range patching for JSON; `toml_edit` for TOML. Your indentation, comments, and line endings stay intact
- **Fast** — concurrent registry lookups across all manifests (`futures::join_all`)
- **Smart range checking** — skips false positives where the resolved version already satisfies the current range
- **Deep scan** — `dcu -d` recursively finds manifests in monorepos, respecting `.gitignore`
- **ncu-compatible UX** — same flags you already know

## Supported Ecosystems

| Ecosystem | Manifest | Registry | Crate |
|-----------|----------|----------|-------|
| Node.js | `package.json` | npm | `dcu-node` |
| Rust | `Cargo.toml` | crates.io | `dcu-rust` |
| Python | `pyproject.toml` | PyPI | `dcu-python` |

## Installation

```bash
# Rust (cargo)
cargo install dcu-cli

# Node.js (npm/bun)
npm install -g @dcu/cli
bun add -g @dcu/cli

# Python (pip/uv)
pip install dependency-check-updates
uv pip install dependency-check-updates
```

## Usage

```bash
# Check for outdated dependencies
dcu

# Apply updates
dcu -u

# Recursively scan monorepo
dcu -d
dcu -d -u

# Target specific update level
dcu -t patch     # only patch bumps
dcu -t minor     # patch + minor bumps
dcu -t latest    # latest stable (default)

# Filter packages
dcu react eslint            # only check these
dcu -x typescript           # exclude these

# Specific manifest
dcu --manifest path/to/Cargo.toml

# JSON output
dcu --format json

# CI mode (exit 1 if updates exist)
dcu -e 2

# Verbose logging
dcu -v    # info
dcu -vv   # debug
dcu -vvv  # trace
```

## Architecture

Follows the [changepacks](https://github.com/changepacks/changepacks) pattern — one crate per language ecosystem, with bridge crates for cross-language distribution:

```
.
├── crates/
│   ├── dcu-cli/       # Binary + async CLI orchestration
│   ├── dcu-core/      # Shared traits (ManifestHandler, RegistryClient, Scanner)
│   ├── dcu-node/      # Node.js: package.json parser + npm registry
│   ├── dcu-rust/      # Rust: Cargo.toml parser (toml_edit) + crates.io
│   ├── dcu-python/    # Python: pyproject.toml parser (toml_edit) + PyPI
│   └── dcu-testkit/   # Test fixtures and helpers
├── bridge/
│   ├── node/          # napi-rs N-API binding → npm distribution (@dcu/cli)
│   └── python/        # maturin bin binding → PyPI distribution (dependency-check-updates)
├── Cargo.toml         # Workspace root
└── package.json       # Bun workspace (build/lint/test scripts)
```

### Format Preservation

- **JSON** (`package.json`): Surgical byte-range replacement — finds exact byte offsets of version values, replaces only those bytes. Indent, line endings, trailing newline, key ordering all preserved byte-for-byte.
- **TOML** (`Cargo.toml`, `pyproject.toml`): `toml_edit` document model preserves comments, table ordering, inline table formatting, and whitespace.

### Shared Traits

Each ecosystem crate implements two core traits from `dcu-core`:

- **`ManifestHandler`** — parse manifest files, collect dependencies, apply format-preserving updates
- **`RegistryClient`** — resolve versions from package registries with concurrency control

### Range Satisfaction

Before reporting an update, the resolver checks if the selected version already satisfies the current range (e.g., `^3` already covers `3.5.1`). This eliminates false positives that plague naive string comparison.

## Development

```bash
# Install dependencies
bun install

# Build everything (CLI + napi + maturin)
bun run build

# Dev build
bun run build:dev

# Lint (clippy + fmt)
bun run lint

# Test
bun run test

# Run CLI
bun run run -- --help
bun run run -- --manifest Cargo.toml -v
```

## Inspirations

- [npm-check-updates](https://github.com/raineorshine/npm-check-updates) — the original `ncu` that inspired dcu's UX and flag design
- [changepacks](https://github.com/changepacks/changepacks) — workspace architecture pattern (crates/\* + bridge/\*), multi-language bridge distribution via napi-rs and maturin, and the overall project structure that dcu follows

## License

MIT
