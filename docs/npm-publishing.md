# npm trusted publishing

The `node-publish` job in `.github/workflows/CI.yml` publishes through GitHub
Actions OIDC. It runs only on pushes to `main`, uses Node.js 24 and npm 11, and
does not use `NPM_TOKEN`. PyPI and crates.io publication are separate.

## Registry setup

Configure a GitHub Actions trusted publisher in the npm settings of **each**
package:

- `@dependency-check-updates/cli`
- `@dependency-check-updates/cli-win32-x64-msvc`
- `@dependency-check-updates/cli-darwin-x64`
- `@dependency-check-updates/cli-linux-x64-gnu`
- `@dependency-check-updates/cli-darwin-arm64`

Use these exact values:

| Field | Value |
| --- | --- |
| Organization or user | `dev-five-git` |
| Repository | `dependency-check-updates` |
| Workflow filename | `CI.yml` |
| Environment name | Leave empty |
| Allowed actions | Enable direct `npm publish` |

The filename is case-sensitive and must not include `.github/workflows/`.
Changing the workflow filename requires updating the registry configuration.
The job already has `id-token: write`. Configure every platform package before
enabling this workflow; configuring only the CLI package is insufficient.
After a successful OIDC release, remove the obsolete repository `NPM_TOKEN`
secret and revoke the old token if no other project uses it.

## Packaging and verification

After building the CLI and collecting all platform artifacts:

```sh
cd bridge/node
bun run pack:publish /absolute/path/to/new-output-directory
```

The script runs N-API preparation with optional publication and GitHub release
creation disabled. It checks that every target has a platform package with the
CLI version, packs the native packages and CLI with `bun pm pack`, and inspects
the actual tarballs for unresolved local dependency ranges and missing entry
points or native binaries. Bun converts `workspace:` ranges before npm sees
the tarballs. An existing output directory is rejected to prevent stale files
from entering a release.

CI completes all checks before publishing anything. It publishes the tarballs
under `native/` first, then the tarball under `cli/`, using
`npm publish <tarball> --access public --ignore-scripts`. Do not replace this
with publication from source directories: npm does not materialize Bun
workspace protocols. `prepublishOnly` now prepares packages without publishing
the platform packages automatically.
Already published versions are skipped for each package individually, so a
partial publication or failed GitHub release asset upload can be retried.

The packaging regression test uses a real native binary, a `workspace:^`
fixture, and the same packing script. It checks the materialized range, rejects
a `file:` dependency, and installs both tarballs offline before running the CLI.

See [npm's trusted publishing documentation](https://docs.npmjs.com/trusted-publishers/).
