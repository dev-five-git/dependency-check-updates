# dependency-check-updates

<!-- Build & Quality -->
[![CI](https://img.shields.io/github/actions/workflow/status/dev-five-git/dependency-check-updates/CI.yml?branch=main&label=CI&logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/actions/workflows/CI.yml)
[![Codecov](https://img.shields.io/codecov/c/github/dev-five-git/dependency-check-updates?logo=codecov&logoColor=white&style=flat-square)](https://codecov.io/gh/dev-five-git/dependency-check-updates)
[![deps.rs](https://deps.rs/repo/github/dev-five-git/dependency-check-updates/status.svg?style=flat-square)](https://deps.rs/repo/github/dev-five-git/dependency-check-updates)
[![License: MIT](https://img.shields.io/github/license/dev-five-git/dependency-check-updates?style=flat-square&color=blue)](./LICENSE)

<!-- Packages & Platforms -->
[![crates.io](https://img.shields.io/crates/v/dependency-check-updates?logo=rust&label=crates.io&style=flat-square)](https://crates.io/crates/dependency-check-updates)
[![npm](https://img.shields.io/npm/v/@dependency-check-updates/cli?logo=npm&label=npm&style=flat-square)](https://www.npmjs.com/package/@dependency-check-updates/cli)
[![PyPI](https://img.shields.io/pypi/v/dependency-check-updates?logo=pypi&logoColor=white&label=PyPI&style=flat-square)](https://pypi.org/project/dependency-check-updates/)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-dea584?logo=rust&style=flat-square)](https://www.rust-lang.org/)
[![Python 3.9+](https://img.shields.io/pypi/pyversions/dependency-check-updates?logo=python&logoColor=white&style=flat-square)](https://pypi.org/project/dependency-check-updates/)
[![Node](https://img.shields.io/node/v/@dependency-check-updates/cli?logo=node.js&logoColor=white&label=node&style=flat-square)](https://www.npmjs.com/package/@dependency-check-updates/cli)

<!-- Downloads -->
[![crates.io downloads](https://img.shields.io/crates/d/dependency-check-updates?logo=rust&label=crates.io%20downloads&style=flat-square)](https://crates.io/crates/dependency-check-updates)
[![npm downloads](https://img.shields.io/npm/dm/@dependency-check-updates/cli?logo=npm&label=npm%20%2Fmonth&style=flat-square)](https://www.npmjs.com/package/@dependency-check-updates/cli)
[![PyPI downloads](https://img.shields.io/pypi/dm/dependency-check-updates?logo=pypi&logoColor=white&label=PyPI%20%2Fmonth&style=flat-square)](https://pypi.org/project/dependency-check-updates/)

<!-- GitHub Community -->
[![GitHub stars](https://img.shields.io/github/stars/dev-five-git/dependency-check-updates?logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/stargazers)
[![GitHub forks](https://img.shields.io/github/forks/dev-five-git/dependency-check-updates?logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/network/members)
[![GitHub issues](https://img.shields.io/github/issues/dev-five-git/dependency-check-updates?logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/issues)
[![GitHub PRs](https://img.shields.io/github/issues-pr/dev-five-git/dependency-check-updates?logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/pulls)
[![Last commit](https://img.shields.io/github/last-commit/dev-five-git/dependency-check-updates?logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/commits/main)
[![Contributors](https://img.shields.io/github/contributors/dev-five-git/dependency-check-updates?logo=github&style=flat-square)](https://github.com/dev-five-git/dependency-check-updates/graphs/contributors)

**Dependency Check & Update** — a fast, multi-ecosystem dependency updater written in Rust.

Like [npm-check-updates](https://www.npmjs.com/package/npm-check-updates), but for every language.

```
$ dependency-check-updates
Checking Cargo.toml
 toml_edit  0.22  ->  0.25.4

Run dependency-check-updates -u to upgrade Cargo.toml
```

## Quick Start (Zero Install)

No install needed — run straight from your package manager's ephemeral runner:

```bash
# Node.js ecosystem
bunx @dependency-check-updates/cli
npx  @dependency-check-updates/cli

# Python ecosystem
uvx dependency-check-updates
pipx run dependency-check-updates
```

All four accept the same flags described in [Usage](#usage).

## Features

- **Multi-ecosystem** — `package.json`, `Cargo.toml`, `pyproject.toml` handled by a single binary
- **Format-preserving** — surgical byte-range patching for JSON; `toml_edit` for TOML. Your indentation, comments, trailing newlines, and key ordering stay intact
- **Fast** — concurrent registry lookups across all manifests via `futures::join_all`
- **Smart range checking** — skips false positives where the resolved version already satisfies the current range (`^3` already covers `3.5.1`)
- **Deep scan** — `-d` recursively finds manifests in monorepos, respecting `.gitignore`
- **ncu-compatible UX** — the same flags you already know from `npm-check-updates`
- **CI-friendly** — `-e 2` exits non-zero when updates exist; `--format json` emits machine-readable output

## Supported Ecosystems

| Ecosystem | Manifest | Registry | Package |
|-----------|----------|----------|---------|
| Node.js | `package.json` | [npm](https://www.npmjs.com/) | [`@dependency-check-updates/cli`](https://www.npmjs.com/package/@dependency-check-updates/cli) |
| Rust | `Cargo.toml` | [crates.io](https://crates.io/) | [`dependency-check-updates`](https://crates.io/crates/dependency-check-updates) |
| Python | `pyproject.toml` | [PyPI](https://pypi.org/) | [`dependency-check-updates`](https://pypi.org/project/dependency-check-updates/) |

## Installation

Every distribution below ships the exact same binary. Pick whichever matches your toolchain.

### Rust (Cargo)

```bash
cargo install dependency-check-updates
```

Installs command: `dependency-check-updates`

### Node.js (npm / bun / pnpm / yarn)

Permanent global install:

```bash
npm  install   -g @dependency-check-updates/cli
bun  add       -g @dependency-check-updates/cli
pnpm add       -g @dependency-check-updates/cli
yarn global add   @dependency-check-updates/cli
```

Installs commands: `dependency-check-updates` **and** `dcu` (short alias).

One-off execution (no install):

```bash
bunx @dependency-check-updates/cli [flags]
npx  @dependency-check-updates/cli [flags]
```

### Python (pip / uv / pipx)

Permanent isolated install:

```bash
pipx install dependency-check-updates
uv tool install dependency-check-updates
```

Install inside a virtualenv:

```bash
pip    install dependency-check-updates
uv pip install dependency-check-updates
```

Installs command: `dependency-check-updates`

One-off execution (no install):

```bash
uvx dependency-check-updates [flags]
pipx run dependency-check-updates [flags]
```

## Usage

Run from a directory containing at least one of `package.json`, `Cargo.toml`, or `pyproject.toml`. Every supported manifest in the current directory is auto-detected.

### Basic

```bash
# Check for outdated dependencies (read-only, nothing is written)
dependency-check-updates

# Apply updates in place (format-preserving)
dependency-check-updates -u

# Recursively scan subdirectories (monorepo-friendly, respects .gitignore)
dependency-check-updates -d
dependency-check-updates -d -u
```

> On Node.js installations the short alias `dcu` works identically — e.g. `dcu -d -u`.

### All Options

```
Usage: dependency-check-updates [OPTIONS] [FILTER]...
```

| Flag | Description | Default |
|---|---|---|
| `[FILTER]...` | Positional package names to include (allowlist; repeatable) | *(all)* |
| `-u, --upgrade` | Write updated versions back to the manifest file | off |
| `-d, --deep` | Recursively scan subdirectories, respecting `.gitignore` | off |
| `-t, --target <LEVEL>` | Version target: `patch` · `minor` · `latest` · `newest` · `greatest` | `latest` |
| `-x, --reject <PATTERN>` | Exclude packages by name (repeatable) | — |
| `--manifest <PATH>` | Operate on a single specific manifest file | *(auto)* |
| `--format <FORMAT>` | Output format: `table` or `json` | `table` |
| `-e, --error-level <N>` | `1` = always exit 0 · `2` = exit 1 when updates exist (CI gate) | `1` |
| `-v, --verbose` | Increase verbosity: `-v` info · `-vv` debug · `-vvv` trace | off |
| `-h, --help` | Print help | — |
| `-V, --version` | Print version | — |

#### `-t, --target` values

| Value | Behavior |
|---|---|
| `patch` | Only patch bumps (e.g., `1.0.1 → 1.0.2`) |
| `minor` | Patch + minor bumps (e.g., `1.0.0 → 1.1.0`) |
| `latest` | Latest **stable** version; prereleases are skipped (**default**) |
| `newest` | Most recently published version by publish date |
| `greatest` | Highest version number, **including prereleases** |

### Examples

```bash
# Target specific update level
dependency-check-updates -t patch           # patch only
dependency-check-updates -t minor           # minor + patch
dependency-check-updates -t latest          # default: latest stable
dependency-check-updates -t greatest        # include prereleases

# Filter packages — positional args act as an include-list
dependency-check-updates react eslint       # only check react and eslint
dependency-check-updates -x typescript      # exclude typescript
dependency-check-updates -x typescript -x lodash

# Operate on a specific manifest
dependency-check-updates --manifest path/to/Cargo.toml
dependency-check-updates --manifest apps/web/package.json

# Machine-readable output for scripting/CI
dependency-check-updates --format json

# CI gate: exit 1 if any updates are available
dependency-check-updates -e 2

# Verbose logging (accumulating)
dependency-check-updates -v    # info
dependency-check-updates -vv   # debug
dependency-check-updates -vvv  # trace

# Combining flags — recursive, patch-only upgrade in a monorepo
dependency-check-updates -d -u -t patch
```

### Zero-Install Examples

Every example above works identically via the ephemeral runners, too:

```bash
bunx @dependency-check-updates/cli                  # check
bunx @dependency-check-updates/cli -u               # apply updates
bunx @dependency-check-updates/cli -d -t minor      # deep scan, minor bumps
bunx @dependency-check-updates/cli react eslint     # filter
npx  @dependency-check-updates/cli --format json

uvx dependency-check-updates
uvx dependency-check-updates -d -u -t patch
pipx run dependency-check-updates --format json
```

## Architecture

Follows the [changepacks](https://github.com/changepacks/changepacks) pattern — one crate per language ecosystem, with bridge crates for cross-language distribution:

```
.
├── crates/
│   ├── cli/           # Binary + async CLI orchestration
│   ├── core/          # Shared traits (ManifestHandler, RegistryClient, Scanner)
│   ├── node/          # Node.js: package.json parser + npm registry
│   ├── rust/          # Rust: Cargo.toml parser (toml_edit) + crates.io
│   ├── python/        # Python: pyproject.toml parser (toml_edit) + PyPI
│   └── testkit/       # Test fixtures and helpers
├── bridge/
│   ├── node/          # napi-rs N-API binding → npm: @dependency-check-updates/cli
│   └── python/        # maturin bin binding → PyPI: dependency-check-updates
├── Cargo.toml         # Workspace root
└── package.json       # Bun workspace (build/lint/test scripts)
```

### Format Preservation

- **JSON** (`package.json`): Surgical byte-range replacement — finds exact byte offsets of version values and replaces only those bytes. Indent, line endings, trailing newline, and key ordering are preserved byte-for-byte.
- **TOML** (`Cargo.toml`, `pyproject.toml`): `toml_edit` document model preserves comments, table ordering, inline-table formatting, and whitespace.

### Shared Traits

Each ecosystem crate implements two core traits from `dependency-check-updates-core`:

- **`ManifestHandler`** — parse manifests, collect dependencies, apply format-preserving updates
- **`RegistryClient`** — resolve versions from package registries with concurrency control

### Range Satisfaction

Before reporting an update, the resolver checks whether the selected version already satisfies the current range (e.g., `^3` already covers `3.5.1`). This eliminates the false positives that plague naive string comparison.

## Development

Build prerequisites:

- Rust 1.85+ (stable toolchain)
- Bun 1.0+ *(or Node.js 18+ with npm)*
- Python 3.9+ with [`maturin`](https://www.maturin.rs/) *(only for the Python wheel step)*
- Windows: Visual Studio 2022 Build Tools (MSVC linker)

```bash
# First-time setup: install JS toolchain deps (@napi-rs/cli, etc.)
bun install

# Build everything (native CLI + napi .node + maturin wheel)
bun run build

# Dev build (faster, unoptimized)
bun run build:dev

# Lint (cargo clippy + rustfmt + bun workspace lints)
bun run lint
bun run lint:fix

# Test (cargo test --workspace + bun workspace tests)
bun run test

# Run CLI from source
bun run run -- --help
bun run run -- --manifest Cargo.toml -v
bun run run:release -- -d
```

## Inspirations

- [npm-check-updates](https://github.com/raineorshine/npm-check-updates) — the original `ncu` that inspired this tool's UX and flag design
- [changepacks](https://github.com/changepacks/changepacks) — the workspace architecture pattern (`crates/*` + `bridge/*`), multi-language bridge distribution via napi-rs and maturin, and the overall project structure

## License

MIT
