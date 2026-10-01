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
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-dea584?logo=rust&style=flat-square)](https://www.rust-lang.org/)
[![Python 3.11+](https://img.shields.io/pypi/pyversions/dependency-check-updates?logo=python&logoColor=white&style=flat-square)](https://pypi.org/project/dependency-check-updates/)
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
$ dcu
Checking Cargo.toml
 toml_edit  0.22  ->  0.25.4

Checking .github/workflows/CI.yml
 actions/checkout    v4  ->  v5
 actions/setup-node  v4  ->  v5

Checking Dockerfile
 node  20-alpine  ->  22-alpine

Run dcu -u to upgrade
```

> `dcu` is a short alias installed alongside `dependency-check-updates`. Both commands are identical — use whichever you prefer.

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

- **Multi-ecosystem** — Node, Rust, Python, GitHub Actions, containers, Android/Gradle and project tool declarations handled by a single binary
- **Format-preserving** — surgical byte-range patching for JSON / YAML / Dockerfiles; `toml_edit` for TOML. Your indentation, comments, trailing newlines, and key ordering stay intact
- **Fast** — concurrent registry lookups across all manifests via `futures::join_all`
- **Smart range checking** — skips false positives where the resolved version already satisfies the current range (`^3` already covers `3.5.1`)
- **Deep scan** — `-d` recursively finds manifests in monorepos, respecting `.gitignore`
- **ncu-compatible UX** — the same flags you already know from `npm-check-updates`
- **Short alias** — type `dcu` instead of `dependency-check-updates`; both are installed by every distribution
- **CI-friendly** — `-e 2` exits non-zero when updates exist; `--format json` emits machine-readable output

## Supported Ecosystems

| Ecosystem | Manifest | Registry | Package |
|-----------|----------|----------|---------|
| Node.js | `package.json` | [npm](https://www.npmjs.com/) | [`@dependency-check-updates/cli`](https://www.npmjs.com/package/@dependency-check-updates/cli) |
| Rust | `Cargo.toml` | [crates.io](https://crates.io/) | [`dependency-check-updates`](https://crates.io/crates/dependency-check-updates) |
| Python | `pyproject.toml` | [PyPI](https://pypi.org/) | [`dependency-check-updates`](https://pypi.org/project/dependency-check-updates/) |
| Android / Gradle | `build.gradle[.kts]`, `settings.gradle[.kts]`, `gradle/libs.versions.toml`, `gradle.properties` | Declared Google Maven, Maven Central, Gradle Plugin Portal | *(built-in)* |
| Gradle distribution | `gradle/wrapper/gradle-wrapper.properties` | Official Gradle release metadata and SHA-256 sidecars | *(built-in)* |
| Project development tools | `package.json` `packageManager`, `.nvmrc`, `.node-version`, `rust-toolchain[.toml]`, `.tool-versions`, `mise.toml`, `.mise.toml` | Official Node/Rust/Bun/Yarn/Temurin release metadata and npm | *(built-in)* |
| GitHub Actions | `.github/workflows/*.yml`, `action.yml` | [GitHub Tags API](https://docs.github.com/rest/repos/repos#list-repository-tags) | *(built-in)* |
| Containers | `Dockerfile`, `compose.yaml` | [OCI Distribution](https://distribution.github.io/distribution/spec/api/) (Docker Hub, ghcr.io, quay.io, …) | *(built-in)* |

### Android and Gradle

The same default lookup, `-d`, `-u`, `--manifest`, targets, positional filters, `--reject`, table and JSON output apply to these files. For a Tauri monorepo, `bunx @dependency-check-updates/cli -d` also finds scripts under `apps/app/src-tauri/gen/android/`, including `buildSrc/build.gradle.kts`. Repeated declarations are all patched; version references patch their original source, keeping the references intact.

Supported static syntax:

| Declaration | Supported forms |
|---|---|
| Maven coordinates | Kotlin `implementation("group:artifact:1.2.3")`, Groovy `implementation 'group:artifact:1.2.3'`; `classpath`, `api`, `*Implementation`, `compileOnly`, `runtimeOnly`, `annotationProcessor`, `kapt`, `ksp` |
| Plugins | `id("example.plugin") version "1.2.3"`, `id 'example.plugin' version '1.2.3'`, and a simple version variable |
| Script version variables | Separate-line `val version = "1.2.3"`, `var` / `def`, `ext.version = '1.2.3'`, `extra["version"] = "1.2.3"`, `val version: String = "1.2.3"`, `val version: String by extra("1.2.3")`, and `$version` / `${version}` references |
| Properties | `version = 1.2.3` or `version: 1.2.3` in the nearest ancestor `gradle.properties`; separate-line `val v = providers.gradleProperty("key").get()`, `val v = property("key")`, or `def v = findProperty('key')`, optionally `as String` or `.toInt()` |
| Version catalog | `[versions]` string values; `[libraries]` `module`, `group` + `name`, or string coordinates; `[plugins]` `id`; inline `version` and `version.ref` |
| Android SDK | Integer `compileSdk`, `compileSdkVersion`, `targetSdk`, `targetSdkVersion`, or a simple integer variable declaration |
| Wrapper | Official `services.gradle.org` / `downloads.gradle.org` distribution URLs ending in `-bin.zip` or `-all.zip`; optional `distributionSha256Sum` |

Gradle scripts are never executed. Comments and unrelated strings are excluded. The listed literal property-provider bindings are resolved by reading project files, not by calling Gradle. Arbitrary computed expressions, concatenation, function calls, unresolved or reassigned variables, provider `.map`/defaults/environment lookups, Maven version ranges/dynamic selectors, classifiers, rich catalog constraints and custom distribution URLs are reported with a reason and preserved. This is a bounded static scanner, not a Groovy/Kotlin interpreter. Simple `implementation(libs.android.webkit)` and `alias(libs.plugins.kotlin.android)` accessors resolve to the nearest ancestor `gradle/libs.versions.toml`; alias hyphens/underscores become dots, and updates patch the catalog source. Custom catalog names, imported convention scripts and arbitrary dependency configurations are outside the supported syntax. Only the documented `String`/`Int` annotations and literal `by extra(...)` initializers are resolved; arbitrary delegates are not.

Repository declarations supported are `google()`, `mavenCentral()`, `gradlePluginPortal()`, and literal `maven("URL")` / `maven { url = uri("URL") }` / Groovy `url 'URL'` forms. Local and ancestor build/settings files supply repository context. By default, only the public Google Maven, Maven Central and Plugin Portal endpoints are queried; use explicit [private Maven access](#private-maven-access) for other literal endpoints. A missing, inaccessible or unsupported declared repository is visible. HTTP 404 in one repository is normal when another declares the artifact; other lookup failures prevent applying a partial result. Android/Kotlin plugin IDs map to their official artifacts; other IDs use Plugin Portal marker coordinates. Gradle credential code, content filters and dynamically calculated URLs are not evaluated.

`compileSdk` and `targetSdk` are shown in the `android-sdk` section and use stable platform packages from Google's SDK repository metadata. `minSdk` is never raised. Updating an API level changes project declarations; installing that SDK remains your build tooling's responsibility.

The wrapper retains the host, escaped colon, `bin`/`all` type and surrounding properties. When a SHA-256 property exists, the matching official distribution checksum must be obtained before applying the update. Failed checksum requests preserve the URL and checksum together and trigger another compatibility check before any project file is written.

### Private Maven access

`--maven-config repositories.json` authorizes exact literal Maven base URLs already declared by the selected build. It does not add repositories, read Gradle credentials, execute scripts or scan global settings. The configuration uses schema 1, rejects unknown fields and duplicate URLs, and is limited to 1 MiB / 128 endpoints:

```json
{
  "schemaVersion": 1,
  "repositories": [
    { "url": "https://repo.example.com/maven", "tokenEnv": "DCU_MAVEN_TOKEN" },
    { "url": "https://repo.example.com/releases", "usernameEnv": "DCU_MAVEN_USER", "passwordEnv": "DCU_MAVEN_PASSWORD" },
    { "url": "https://repo.example.com/public" }
  ]
}
```

Supply either a Bearer token environment-variable name, both HTTP Basic credential names, or no credentials for an explicitly authorized anonymous endpoint. Never put secret values in the file or URL. Missing variables and invalid headers fail before writes. HTTPS is required except for explicit loopback HTTP repositories. Embedded URL credentials, query strings and fragments are rejected. Private requests refuse all redirects; point to the final metadata endpoint rather than forwarding credentials. Authorization headers are sensitive, cache keys include credentials, and diagnostic messages omit credential values and request URLs. Private metadata uses the same bounded response/in-flight cache policy in a separate redirect-disabled client.

```bash
dcu -d --maven-config repositories.json --format json-report
dcu -d -u --maven-config repositories.json --fail-on-incomplete
```

### Development tool declarations and local installations

Project tool pins are shown in the `toolchain` section. Supported tools in `.tool-versions` and mise's `[tools]` table are `node` / `nodejs`, `npm`, `pnpm`, `yarn`, `bun`, `rust`, and `java` / `jdk`. Numeric versions, a leading `v`, short release-line pins, and Temurin's `temurin-` prefix are preserved. Vendor-specific JDK formats other than Temurin, multiple installed-version lists and complex mise tool tables are reported as unsupported.

Moving channels such as `stable`, `beta`, `nightly`, `lts/*` and `lts/jod` remain channels and are reported separately from numeric pins. A project's `engines` and other support ranges are not rewritten into a new minimum. Unsupported compound constraints are preserved. Supported hidden files are included by `-d`; `.gitignore` / `.ignore`, other hidden directories, and existing dependency/build directory exclusions still apply.

`packageManager` supports npm, pnpm, Yarn and Bun. Existing Corepack hashes (`sha1`, `sha224`, `sha256`, `sha384`, `sha512`) are recalculated from the selected release's bytes: npm/pnpm/Yarn Classic tarballs, or the official `yarn.js` for Yarn 2+. The algorithm is retained. Failure or an unknown integrity scheme preserves the declaration; a hash is never silently removed. Bun declarations without hashes are supported; hashed Bun declarations are preserved because no Corepack integrity convention applies. URL-based and escaped/ambiguous `packageManager` declarations are outside the supported syntax.

```bash
# Project declarations, including generated Tauri Android scripts
bunx @dependency-check-updates/cli -d
dcu -d -u --reject junit
dcu -d androidx.webkit pnpm node
dcu --manifest apps/app/src-tauri/gen/android/app/build.gradle.kts -u -t patch
dcu --manifest gradle/wrapper/gradle-wrapper.properties -u

# Installed tool versions; works even without a project manifest
dcu --local-tools
dcu --local-tools node bun pnpm rust jdk --format json
```

`--manifest` can also update a referenced ancestor property or catalog source. Other discovered Gradle declarations participate in compatibility and shared-source checks (respecting ignore/build-directory exclusions), without entering the update allowlist. A shared source is preserved if consumers select different targets, a consumer is unselected/filtered/rejected, or any consumer's lookup fails. Even repeated declarations of the same artifact retain their individual repository context.

Context discovery is lazy: a Node/Rust/Python-only selection does not recursively scan for Gradle files. A targeted Gradle selection scans the nearest build and relevant ancestor sources; if a shared source belongs to an ancestor build, that scope expands to find its other consumers. A deep scan reuses the files already discovered. Nearest settings/wrapper boundaries stop repository inheritance from unrelated builds, and catalog aliases use the consuming build's repositories. Ignore rules still apply to these context scans. Arbitrary `includeBuild(...)` and applied external convention scripts are not followed: they are reported and leave compatibility unverified, even if the visible pins otherwise match.

`--local-tools` runs only bounded version queries for Node, Bun, pnpm, Rust and JDK, reporting missing tools, failed queries and failed registry lookups. It compares installed versions with release metadata and provides update guidance. Probes run in a temporary neutral directory; Corepack project selection, network downloads and automatic pinning, and rustup automatic toolchain installation are disabled. Project tool pins therefore do not select or install the probed toolchain. It cannot be combined with `-u`, `--manifest`, `-d` or cleanup flags. `-u` continues to modify project files only. Local updates remain explicit user actions: [`bun upgrade`](https://bun.sh/docs/installation), [`pnpm self-update`](https://pnpm.io/cli/self-update), or `rustup update stable`; Node and JDK have no universal self-update command, so use the [Node installer](https://nodejs.org/en/download) or [Temurin installation instructions](https://adoptium.net/installation).

### Compatibility and report statuses

Reports distinguish the latest published release (`latest`), the target selection (`selected`), and a safe project change (`to`). Before writing, DCU checks the complete proposed combination within each Gradle build, including ancestor wrapper and declared JDK pins. AGP/Kotlin versions referenced from ancestor properties or catalogs are checked in their consuming builds. Definite conflicts preserve the coupled AGP/Gradle/Kotlin/JDK/SDK changes, including their ancestor sources. Missing required pins and combinations beyond the checked tables are `unverified`; this is visible and does not claim universal build compatibility. A plain Gradle/JDK project does not require Android pins; standalone tool declarations do not require a Gradle compatibility check.

The bounded rules cover AGP 8.0–8.13 and 9.0–9.4 minimum Gradle versions, their JDK 17 requirement, integer SDK 34–37 minimum AGP versions, Kotlin 1.9.20–1.9.25 and explicitly listed 2.0–2.4 KGP compatibility ranges, bundled Kotlin bytecode/R8 minimum AGP requirements, and Gradle's documented runtime support for JDK 17–27 (JDK 27 requires Gradle 9.8.0). Kotlin combinations above fully supported maximums are unverified. AGP 9 plus an applied external Kotlin Android/KMP plugin is blocked until an explicit migration; DCU does not edit plugin application or opt-out configuration. Custom R8 overrides, JDK selection by Gradle/Android Studio, plugin migrations and general dependency solving are outside these checks.

Rules are based on the official [AGP/Gradle and SDK requirements](https://developer.android.com/build/releases/about-agp), [Kotlin compiler requirements](https://developer.android.com/build/kotlin-support), [KGP compatibility ranges](https://kotlinlang.org/docs/gradle-configure-project.html), [Gradle Java matrix](https://docs.gradle.org/current/userguide/compatibility.html) and [AGP 9 migration notes](https://developer.android.com/build/releases/agp-9-0-0-release-notes). The versioned [built-in snapshot](crates/cli/data/compatibility-v1.json) records its review date and sources, and ships inside the CLI crate. The checker consumes this data rather than hard-coded release tables.

`--compatibility-file rules.json` extends coverage without rebuilding DCU. Use the snapshot's schema 1 layout: required `verifiedAt` and official `sources`, optional `agp` (major.minor -> minimum Gradle/JDK), `sdk` (API -> minimum AGP), `jdk` (major -> minimum Gradle), and `kotlin` (explicit version ranges, fully supported Gradle/AGP bounds and minimum bytecode AGP). Empty/omitted maps add no rules. Invalid versions, inverted/overlapping ranges, unknown fields and attempts to replace built-in conditions are rejected before writes. Identical repeated built-in rules are allowed. Review new conditions against their official documentation before supplying the file; URLs and dates are provenance, not authentication. Reports explicitly identify user-supplied rule extensions as not independently authenticated. Unsupported future releases are never guessed from a previous release's conditions.

`--format json` continues to emit one JSON array on stdout across all manifests, without table headers/footers. Rows include `manifest`, `name`, `section`, `from`, `latest`, `selected`, `compatible`, `to`, `status`, `reason`, `compatibility`, `selectionPolicy`, and `updated`. Statuses are `update`, `current`, `channel`, `unsupported`, `failed`, `blocked`, or `unverified`; installed-tool rows also use `missing`. Failed/unhandled declarations are never represented as “all up to date”. See [JSON contracts](#json-contracts) for a versioned envelope and an explicit legacy migration format.

`newest` uses Gradle build timestamps, Node release dates, Bun release timestamps and npm publish dates. Maven, SDK platform and JDK metadata in this scanner have no per-version publish-date selection, so `newest` falls back to `greatest`; modern Yarn tags use the same fallback. Rust uses the current official stable manifest for `latest` / `newest` / `greatest` and the first 100 GitHub releases for `minor` / `patch`. This does not offer exhaustive historical or prerelease Rust selection.

### Strict CI checks

`--fail-on-incomplete` exits 2 when a selected declaration is unsupported, failed, blocked, unverified, or a selected local tool is missing. Unverified compatibility counts even when a row otherwise has an available update. Run-level diagnostics, including pending recovery receipts, also count. Intentional channels such as `stable`/LTS alone do not fail, and excluded items are not counted. With `-u`, any incomplete result aborts the entire update batch before writes and skips requested cleanup.

`--strict-compatibility` blocks unverified coupled Gradle/AGP/Kotlin/JDK/SDK changes. Independent library and tool updates may still proceed. It does not change version-target selection or claim to solve dependency compatibility; use it together with `--fail-on-incomplete` to require a fully checked batch. It cannot be combined with `--local-tools`.

#### Verified combination suggestions

Gradle-tool rows also expose `compatible`: a candidate from a combination that passes the documented rules. A normal query leaves `selected` (the `--target` result) and project files unchanged. Use `--compatible -u` explicitly to select these suggestions instead of the individually selected latest tool versions. Independent libraries and standalone tools still use ordinary target selection. Filters, `--reject`, numeric prefixes and short JDK pin precision are retained; channels remain channels, and existing pins are never downgraded.

```bash
dcu -d --format json-report                    # compare latest, selected and compatible
dcu -d --compatible -u --fail-on-incomplete     # apply a verified retained batch
dcu -d --compatible --target minor             # restrict candidate release lines
```

This is a bounded, deterministic search, not a general dependency solver or a build execution: published candidates at or below the ordinary selection are tried in descending AGP, Kotlin, Gradle, JDK and SDK order. There are at most 64 candidates per dimension plus a current-pin fallback and 4,096 search steps per connected build component. Shared sources are checked across their consumers; separate components are checked independently. A suggestion is the first verified combination in that order, not a promise of a globally optimal version vector. Missing pins, dynamic build connections or exhausted searches produce no verified suggestion. An exhausted step budget is a diagnostic; `--compatible` leaves affected coupled changes unapplied when no verified combination is found. Normal compatibility/shared-source guards run again after selection and integrity preparation.

Rule dates are validated as Gregorian dates, including leap years. `json-report.compatibilityRules` records each built-in/extension source set, `verifiedAt`, `userSupplied`, UTC `ageDays`, `stale` and `future`. Data older than 180 days or future-dated data makes compatibility unverified and emits a diagnostic; strict compatibility and verified-combination selection cannot approve it. User-provided official-looking source URLs remain user-reviewed provenance, not independently authenticated evidence.

Exit policy is shared by the binary and bridges: execution/apply failures exit 1; otherwise `--fail-on-incomplete` takes priority and exits 2 for incomplete checks; otherwise `-e 2` exits 1 for available updates; otherwise exit 0. Argument errors also exit 2. Without the new flags, item-level lookup failures remain visible but keep the existing exit policy.

```bash
# Read-only CI check: distinguish incomplete checks (2) from available updates (1)
dcu -d --fail-on-incomplete -e 2 --format json-report

# Apply only a fully checked batch; no cleanup or writes on incomplete checks
dcu -d -u --strict-compatibility --fail-on-incomplete --format json-report

# Missing or failed installed-tool probes fail the check without installing anything
dcu --local-tools --fail-on-incomplete --format json-report
```

### Recoverable project updates

All ecosystem patches are prepared before any project file is changed. DCU stages each replacement and an original-byte backup beside its target, checks the original bytes again, and writes `.dcu-transaction-active.json` in the working directory. The receipt and stable sibling `.dcu-lock-<hash>` files use OS locks: another DCU run cannot update the same target even from a different working directory. Lock files remain as empty coordination identities; the OS releases their locks when the process exits. Do not delete them while an updater is running. Each file is atomically replaced; `updated` becomes true only after the whole batch commits. Regular apply failures roll back already-replaced files. A file changed by another process is never overwritten during rollback: backups and the receipt remain, and the report says `recovery-required`.

The batch is recoverable, **not crash-atomic**: existing independent files cannot be made visible as one atomic filesystem operation. A subsequent query reports the receipt without modifying files; `-u` refuses to proceed until recovery is resolved. Use the explicit recovery command below: it preflights every current target, validates source SHA-256 hashes and scoped artifact paths, refuses live transactions and external edits, and requires no registry/network requests. Rollback restores recorded original bytes; finish completes the previously prepared replacements, without selecting newer versions. Already-restored/completed files are accepted, so recovery can be retried. Original backups are copied rather than consumed during recovery. Corrupt/missing required artifacts leave the receipt intact. Recovery never runs implicitly during a read-only query.

```bash
dcu --recover rollback --format json-report  # restore the recorded original batch
dcu --recover finish --format json-report    # finish the recorded prepared batch
```

Run from the directory that contains the receipt. Recovery cannot combine with filters, manifests, `-u`, `-d`, cleanup, local probes or registry/rule configuration. `json-report` records `rolled-back`, `committed`, `no-changes` or `recovery-required`; recovery failures exit 1. Updates and recovery targets must stay within the working directory tree (canonical parent paths are checked). Run from an external manifest's own project root instead of writing outside the recovery scope. After an external edit, deliberately reconcile it with the recorded versions first; no command blindly overwrites it or discards the receipt.

Successful commits and ordinary successful rollbacks remove their staged/backup/receipt artifacts (stable sibling lock identities remain). Symbolic-link and hard-linked targets are rejected; Windows read-only targets are also rejected. Unix staging preserves owner/group, mode and exposed extended attributes/ACLs, refusing the update if those cannot be copied. Unix parent directories are synced after staging and replacement. Windows stages and backups receive the source DACL/protection before any content is written; native replacement retains the target's ACL and creation metadata. These guards coordinate cooperating DCU processes; unrelated editors do not honor DCU's locks, so there remains a narrow comparison/replacement race. DCU does not claim multi-file power-loss atomicity or overwrite permissions to force a write.

Before replacing targets, DCU syncs sibling `.dcu-pending-<identity>.json` links to the recovery receipt. Another DCU invocation detects these links even from a different working directory and refuses to modify an interrupted batch. Updates with pending recovery evidence stop before registry queries. Read-only queries report the evidence and do not remove it. A link whose receipt was already removed is reclaimed only during a later update while holding the target lock. Recovery still runs from the receipt's directory and validates recorded targets/artifacts before writing. If files were committed but finalization fails, the report retains their `updated` state and returns a failure instead of claiming no write occurred.

Requested lockfile/environment cleanup happens only after non-aborted, non-failed processing; it remains an explicitly destructive action outside the rollback transaction. Android/tool support adds no global-cache or installation deletion targets.

Cleanup failures are `cleanup-failed` diagnostics with the target path and exit 1; successful removals remain listed. A cleanup failure does not undo committed project updates, so inspect both `updated`/`applyOutcome` and diagnostics. Filesystem traversal and invalid ignore-rule errors also fail the invocation rather than silently hiding manifests. The CLI and context discovery use the checked Scanner APIs; legacy Vec-returning Scanner helpers remain available for callers explicitly accepting a best-effort scan. Integrity preparation failures block only the matching declaration and sidecar, preserving independent updates and already-current statuses in the same file. `--fail-on-incomplete` still aborts the entire batch before writes.

### JSON contracts

All JSON modes emit exactly one value on stdout; verbose logs and cleanup progress use stderr. Project lookup results are read-only unless `-u` is supplied.

| Format | Contract |
|---|---|
| `json` | Declaration-status array (retained); empty selections produce `[]`. Run-level diagnostics go to stderr. |
| `json-report` | Envelope with `schemaVersion: 2`, `summary`, `items`, `diagnostics`, `compatibilityRules`, and `applyOutcome`; see [report-v2.schema.json](crates/cli/schemas/report-v2.schema.json). Fatal execution errors also emit a report envelope. |
| `json-legacy` | Update-only `{ "package": "version" }` object for at most one effective project manifest; empty selections produce `{}`. Incomplete results are explained on stderr. |

`latest` is the latest published release; `selected` follows `--target`; `compatible` is a verified coupled-tool combination candidate or null; `to` is the retained project update or null when no update is planned. A non-null `to` is not proof of a write: check `updated` and `applyOutcome`. Local rows instead contain `scope: "local"`, `installed`, `latest`, `selected`, `status`, `reason`, `updateCommand`, and `updated: false`.

Summary counts are declaration rows, not unique packages or files: `checked`, planned `updates`, committed `updated`, and `incomplete` (incomplete rows plus run diagnostics). `manifests` counts effective manifest jobs, including referenced sources and empty context manifests. Outcomes are `not-requested`, `no-changes`, `committed`, `aborted`, `rolled-back`, or `recovery-required`.

`json-legacy` is a migration option for consumers of the old update-only map, not a lossless monorepo report. Multiple effective manifests (even empty context manifests), local-tool reports, or conflicting selected versions for the same name are rejected **before writes**. Repeated names with the same target collapse to one map entry. Use `json`/`json-report` to retain per-file statuses and failed checks; do not interpret an empty legacy map as a successful complete scan.

```bash
dcu -d --format json                 # stable declaration array
dcu -d --format json-report          # versioned automation contract
dcu --manifest package.json --format json-legacy
dcu --local-tools --format json-report
```

### Request sharing and limits

One invocation shares in-flight public metadata requests and responses across repeated declarations, ordinary npm packages and `packageManager`, Maven libraries/plugins, tool pins and integrity sidecars. npm, crates.io and PyPI share the same bounded request layer. URL, explicit request headers and response-size limit are part of the key; versions are still selected independently for each pin and target. Failures are shared without changing the package name in each diagnostic. A new invocation starts fresh: no stale cross-run or user-wide disk cache is created.

The shared layer allows 10 concurrent requests, retains at most 512 keys and 64 MiB of body data, and bounds individual metadata/integrity responses to 32/50 MiB. When retention is saturated, new lookups still run with response/concurrency bounds. GitHub Actions and OCI images retain their repository/auth-aware batch implementation, now batched across manifests to avoid duplicate tag queries; authenticated token exchanges are not blindly cached by URL.

Unretained responses do not consume the retained-body budget: reaching the key limit cannot falsely exhaust the byte budget. Candidate search reuses the same repository/header-aware cache as normal lookup.

### GitHub Actions specifics

- Discovers every `*.yml` / `*.yaml` under `.github/workflows/` and any `action.yml` / `action.yaml` at the repo root automatically. Composite actions nested under `.github/actions/**/` are picked up with `-d`.
- Scans `uses: owner/repo@ref` directives. Refs without version digits — `@main`, `@master`, branch names, and full commit SHAs — are **left untouched** on purpose; they pin a moving target intentionally.
- Tag prefix is preserved: `@v5` updates to `@v6` (major float), `@v5.1.0` updates to `@v6.0.0` (full precision). Bare semver without the `v` (`@1.2.3`, `@5`) is recognised and tracked the same way.
- Duplicate rows are collapsed in the output — if `actions/checkout@v5` appears in 12 jobs, you see one row, not twelve. The patch engine still updates every occurrence in the file.
- **Rate limit**: unauthenticated runs use GitHub's 60 req/hr ceiling. Hitting it produces an explicit error pointing to the fix — set `GITHUB_TOKEN` (or `GH_TOKEN`) in your environment to raise the limit to 5 000 req/hr.
- Tag fetch is bounded to the **first 100 tags** per action (newest-first). This comfortably covers every mainstream action; deliberately not paginating keeps API consumption predictable so deep scans don't spike into the rate-limit ceiling.

### Container image specifics

Scans Dockerfile `FROM` instructions and the `image:` key of Compose services. Workflow job `container:` / `services:` images are picked up too — a single `.github/workflows/CI.yml` can have its `uses:` refs resolved against GitHub and its `image:` pins against a container registry in the same run.

**Build variants are never crossed.** A container tag is a version *plus* a variant, and bumping `node:20-alpine` to `node:22` would silently swap Alpine for Debian. Candidate tags are grouped by the verbatim suffix after the leading numeric run, and only tags in the same group are ever considered:

```
node:20-alpine      →  node:22-alpine       (not node:22)
python:3.12-slim    →  python:3.13-slim
postgres:16.0       →  postgres:16.15       (-t minor)
```

Your pin precision is preserved as long as a real tag backs it: `node:20` becomes `node:22`, not `node:22.3.0`. If the registry never published the shorter form, the tag is escalated to the shortest one that actually exists, so the emitted tag always pulls.

**Registries.** Any OCI Distribution registry works from the same code path — Docker Hub, `ghcr.io`, `quay.io`, `mcr.microsoft.com`, `public.ecr.aws`, or a self-hosted `localhost:5000` (plain HTTP for `localhost` / `127.0.0.1`, HTTPS otherwise). Public images authenticate through the registry's anonymous Bearer-token exchange automatically; private repositories are reported as an error rather than guessed at.

**Left untouched on purpose** — each of these means you opted out of tag tracking:

| Pin | Why it is skipped |
|---|---|
| `FROM node` · `image: redis` | No tag: an implicit `latest`, a moving target |
| `:latest` · `:bookworm` · `:stable` | Not a version |
| `node:20@sha256:…` | The digest decides what is pulled; moving the tag alone changes nothing |
| `node:${NODE_VERSION}` · `app:${TAG}` | The real value lives in a build arg or `.env` |
| `FROM builder` | A multi-stage build stage, not an image |
| `app:1a2b3c4` | A build hash, same heuristic that skips commit SHAs in workflows |

Discovery covers `Dockerfile`, `Dockerfile.<suffix>`, `<prefix>.Dockerfile`, `compose.y(a)ml`, and `docker-compose.y(a)ml` including profile variants (`docker-compose.override.yml`). Plain `dcu` probes the canonical names at the root; `-d` finds the rest anywhere in the tree.

`-t newest` falls back to `greatest`: the OCI tag list carries no publish dates, and recovering them would cost one manifest fetch per tag.

## Installation

Every distribution below ships the exact same binary. Pick whichever matches your toolchain.

### Rust (Cargo)

```bash
cargo install dependency-check-updates
```

Installs commands: `dependency-check-updates` **and** `dcu` (short alias).

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

Installs commands: `dependency-check-updates` **and** `dcu` (short alias).

One-off execution (no install):

```bash
uvx dependency-check-updates [flags]
pipx run dependency-check-updates [flags]
```

## Usage

Run from a directory containing a supported manifest or tool declaration, including `package.json`, `Cargo.toml`, `pyproject.toml`, Gradle build/settings files, `.nvmrc`, `.tool-versions`, `.github/workflows/*.yml`, `Dockerfile`, or `compose.yaml`. Canonical manifests, root `gradle/` catalogs/wrappers and supported tool declarations are auto-detected; use `-d` for nested projects. `--local-tools` also works without a project manifest.

All examples below use the short `dcu` alias. The long form `dependency-check-updates` works identically.

### Basic

```bash
# Check for outdated dependencies (read-only, nothing is written)
dcu

# Apply updates in place (format-preserving)
dcu -u

# Recursively scan subdirectories (monorepo-friendly, respects .gitignore)
dcu -d
dcu -d -u
```

### All Options

```
Usage: dcu [OPTIONS] [FILTER]...
```

| Flag | Description | Default |
|---|---|---|
| `[FILTER]...` | Positional package names to include (allowlist; repeatable) | *(all)* |
| `-u, --upgrade` | Apply project-file updates as a recoverable multi-file batch | off |
| `-d, --deep` | Recursively scan subdirectories, respecting `.gitignore` | off |
| `-t, --target <LEVEL>` | Version target: `patch` · `minor` · `latest` · `newest` · `greatest` | `latest` |
| `-x, --reject <PATTERN>` | Exclude packages by name (repeatable) | — |
| `--manifest <PATH>` | Operate on a single specific manifest file | *(auto)* |
| `--format <FORMAT>` | `table`, `json` array, `json-report` v2 envelope, or single-manifest `json-legacy` map | `table` |
| `--local-tools` | Read-only installed Node/Bun/pnpm/Rust/JDK versions, release comparison and update guidance | off |
| `--recover <MODE>` | `rollback` or `finish` a recorded interrupted update, verifying hashes without network requests | off |
| `--maven-config <PATH>` | Explicit literal Maven endpoints and credential environment-variable names (schema 1 JSON) | off |
| `--compatibility-file <PATH>` | Validated rule extensions; cannot replace built-in compatibility conditions | off |
| `--fail-on-incomplete` | Exit 2 for incomplete checks; with `-u`, abort all writes and cleanup | off |
| `--strict-compatibility` | Block unverified coupled Gradle-tool changes; independent updates may proceed | off |
| `--compatible` | Select a bounded verified Gradle-tool combination; latest/target remain separately visible | off |
| `--remove-lockfile` | Delete lockfiles next to each manifest so the package manager re-resolves transitive deps on the next install | off |
| `--remove-installed` | Delete installed-dependency directories next to each manifest for a clean install | off |
| `--rm` | Shortcut for `--remove-lockfile --remove-installed` — wipes both in one go | off |
| `-e, --error-level <N>` | `1` = no update-availability failure; `2` = exit 1 when updates exist (execution/strict failures still apply) | `1` |
| `-v, --verbose` | Increase verbosity: `-v` info · `-vv` debug · `-vvv` trace | off |
| `-h, --help` | Print help | — |
| `-V, --version` | Print version | — |

#### `-t, --target` values

| Value | Behavior |
|---|---|
| `patch` | Only patch bumps (e.g., `1.0.1 → 1.0.2`) |
| `minor` | Patch + minor bumps (e.g., `1.0.0 → 1.1.0`) |
| `latest` | Latest **stable** version; prereleases are skipped (**default**) |
| `newest` | Most recently published version **by publish date** (npm `time`, crates.io `created_at`, PyPI upload time). For GitHub Actions this falls back to `greatest` — the Tags API exposes no per-tag dates. |
| `greatest` | Highest version number, **including prereleases** |

> All five targets apply to every ecosystem, including Python (`pyproject.toml` / `PyPI`), which resolves PEP 440 versions from the full release list.

### Examples

Cleanup remains scoped to the existing manifest-sibling lockfiles and installed directories listed by `--help`: Node lockfiles and `node_modules`, `Cargo.lock` and `target`, Python lockfiles and local environments. Android/Gradle/tool declarations add no cleanup targets. `--rm` never removes user-wide Gradle caches, wrapper distributions, Android SDKs, Corepack caches, rustup toolchains or other global tool installations. Deletion is permanent; the removed lockfiles/environments must be regenerated by the corresponding package manager.

```bash
# Target specific update level
dcu -t patch           # patch only
dcu -t minor           # minor + patch
dcu -t latest          # default: latest stable
dcu -t greatest        # include prereleases

# Filter packages — positional args act as an include-list
dcu react eslint       # only check react and eslint
dcu -x typescript      # exclude typescript
dcu -x typescript -x lodash

# Filter GitHub Actions by owner — same filter syntax works across ecosystems
dcu actions            # only actions/checkout, actions/setup-node, …

# Operate on a specific manifest
dcu --manifest path/to/Cargo.toml
dcu --manifest apps/web/package.json
dcu --manifest .github/workflows/CI.yml
dcu --manifest services/api/Dockerfile
dcu --manifest docker-compose.override.yml

# Machine-readable output for scripting/CI
dcu --format json
dcu -d --format json-report
dcu --manifest package.json --format json-legacy

# CI gate: exit 1 if any updates are available
dcu -e 2
dcu -d --fail-on-incomplete -e 2 --format json-report
dcu -d -u --strict-compatibility --fail-on-incomplete

# Verbose logging (accumulating)
dcu -v    # info
dcu -vv   # debug
dcu -vvv  # trace

# Combining flags — recursive, patch-only upgrade in a monorepo
dcu -d -u -t patch

# Force a full transitive refresh: bump manifests, wipe lockfiles, wipe
# installed copies. The next `bun install` / `cargo build` / `uv sync`
# rebuilds the dep tree from scratch and picks up the latest dep-of-dep.
dcu -u --rm                                      # shortcut for both removals
dcu -d -u --rm                                   # same, monorepo-wide
dcu -u --remove-lockfile                         # lockfiles only, keep installed
dcu -u --remove-installed                        # installed only, keep lockfiles

# Files / directories removed (per manifest discovered):
#   package.json   → bun.lock, bun.lockb, package-lock.json, pnpm-lock.yaml,
#                    yarn.lock, node_modules/
#   Cargo.toml     → Cargo.lock, target/
#   pyproject.toml → uv.lock, poetry.lock, Pipfile.lock, .venv/, venv/,
#                    __pypackages__/, .tox/, .nox/

# GitHub Actions: pin a higher rate limit by exporting a token
GITHUB_TOKEN=ghp_xxx dcu -d -u
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
│   ├── cli/           # Binary + async CLI orchestration (installs `dcu` + `dependency-check-updates`)
│   ├── core/          # Shared traits (ManifestHandler, RegistryClient, Scanner)
│   ├── node/          # Node.js: package.json parser + npm registry
│   ├── rust/          # Rust: Cargo.toml parser (toml_edit) + crates.io
│   ├── python/        # Python: pyproject.toml parser (toml_edit) + PyPI
│   ├── github/        # GitHub Actions: workflow YAML parser + GitHub Tags API
│   └── docker/        # Containers: Dockerfile / Compose scanners + OCI registry
├── bridge/
│   ├── node/          # napi-rs N-API binding → npm: @dependency-check-updates/cli
│   └── python/        # maturin bin binding → PyPI: dependency-check-updates
├── Cargo.toml         # Workspace root
└── package.json       # Bun workspace (build/lint/test scripts)
```

### Format Preservation

- **JSON** (`package.json`): Surgical byte-range replacement — finds exact byte offsets of version values and replaces only those bytes. Indent, line endings, trailing newline, and key ordering are preserved byte-for-byte.
- **TOML** (`Cargo.toml`, `pyproject.toml`): `toml_edit` document model preserves comments, table ordering, inline-table formatting, and whitespace.
- **YAML** (`.github/workflows/*.yml`, `action.yml`, `compose.yaml`): Line-based `uses:` / `image:` scanning with byte-range replacement of only the `@ref` or `:tag` portion. Anchors, comments, blank lines, quoting style, and unrelated `@main` / `:latest` / digest pins are never touched.
- **Dockerfile**: Line-based `FROM` scanning with byte-range replacement of only the tag. `--platform` flags, `AS <stage>` tails, and the `# syntax=` directive survive byte-for-byte.
- **Gradle and project tools**: Static byte-range patches to numeric declarations and resolved source spans preserve comments, quotes, indentation, prefixes and CRLF; wrapper URL/checksum and package-manager version/integrity are prepared together.

The CLI combines prepared patches into a recoverable transaction and uses one typed run report for table/JSON output and exit policy. Scanner context is scoped and lazy; the shared registry metadata layer is bounded and invocation-local. There is no Gradle execution or general dependency solver.

### Shared Traits

Each ecosystem crate implements two core traits from `dependency-check-updates-core`:

- **`ManifestHandler`** — parse manifests, collect dependencies, apply format-preserving updates
- **`RegistryClient`** — resolve versions from package registries with concurrency control

### Range Satisfaction

Before reporting an update, the resolver checks whether the selected version already satisfies the current range (e.g., `^3` already covers `3.5.1`). This eliminates the false positives that plague naive string comparison.

## Development

The Tauri-style fixture in `crates/cli/tests/fixtures/tauri` exercises real discovery, repeated Gradle declarations, catalogs/properties, hidden tool files and existing ecosystems. Tests materialize `Cargo.toml.fixture` as a real `Cargo.toml` in a temporary monorepo; the template suffix prevents Cargo packaging from excluding the fixture as a nested Rust package. Registry responses are fixed in Rust, Node bridge and installed Python-wheel tests. Regression coverage includes read-only scans, CRLF/comment preservation, wrapper checksums, verified combination selection, compatibility/shared-source blocking, scoped discovery and traversal errors, request sharing and saturated-cache accounting, JSON contracts/exit codes, per-declaration integrity failure, cleanup failures, multi-file rollback, explicit crash recovery across working directories, finalization failures, external edits, credential isolation and redirects.

The reusable [project-regressions workflow](.github/workflows/project-regressions.yml) runs locked workspace tests, lint, format, both CLI aliases, fixed-response native bridge tests, packed npm/native-package installation and installed wheel query/update on Linux, macOS and Windows. It also runs the complete Rust 1.88 workspace tests and verifies all distributable Rust crate archives. The main CI calls this workflow and requires it before `changepacks`, thereby gating dependent package publication. Platform-specific filesystem tests run only on their OS; remote CI results are separate from a local Windows run. Packaging checks do not publish packages.

```bash
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo +1.88.0 test --locked --workspace
cargo package --locked --workspace --exclude dependency-check-updates-napi --exclude dependency-check-updates-python-bridge --allow-dirty
bun install --frozen-lockfile --ignore-scripts
bun run --cwd bridge/node test
uvx --from 'maturin>=1.8,<2' maturin build --locked --manifest-path bridge/python/Cargo.toml --out target/wheel-smoke
uv run --no-project python bridge/python/test_wheel.py target/wheel-smoke
```

Build prerequisites:

- Rust 1.88+ (matches the locked NAPI/benchmark dependencies; tested as the minimum workspace toolchain)
- Bun 1.0+ *(or Node.js 18+ with npm)*
- Python 3.11+ with [`maturin`](https://www.maturin.rs/) *(only for the Python wheel step)*
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
