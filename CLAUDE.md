# Claude.md

## Dependency management

**Use nix for all dependencies except node_modules.** Rust toolchain,
postgres, libclang, openssl, the CLI binary itself — all come through
`flake.nix`. The dev shell (`nix develop`) and CI checks
(`nix-fast-build .#checks`) read from the same source of truth.

Exceptions, with reasoning, go below in this section. Don't add ad-hoc
package managers or globally-installed tools without writing the
exception down first.

### Exceptions

- **`packages/runtime/node_modules/`** — TS deps live in bun's workspace
  node_modules and are pinned by `bun.lock`. Reaching them is what `bun
  install` is for. The flake's `mkTscCheck` derivation calls `bun
  install --frozen-lockfile` inside `__noChroot = true` so the lockfile
  stays authoritative without bypassing nix for anything else.

- **None for the release CLI binary anymore.** Release builds run
  inside `nix develop .#release` (see `flake.nix`). On Linux the
  shell targets `*-unknown-linux-musl`, so the binary is fully static
  — no dynamic loader, runs on NixOS / Alpine / glibc / anything.
  On darwin the shell uses the native rust toolchain; mach-O against
  system frameworks is already portable across Macs. The previous
  "native rust off nix" exception (Ubuntu apt libclang +
  `dtolnay/rust-toolchain`) is gone.

## Bun vs npm

**Use bun for everything except npm publishing.** Install, build,
scripts, test runners — all `bun`. The single exception is
`npm publish`: bun's publish path doesn't yet exchange the GitHub
Actions OIDC id-token for an npm trusted-publishing access token, so
`apps.publish-platform-binary` and `apps.publish-meta` in `flake.nix`
shell out to `npm publish --provenance`. Switch back to `bun publish`
once bun supports OIDC.

The `publishEnv` in `flake.nix` carries `pkgs.nodejs_24` precisely
because nodejs_24 ships npm 11.5+ (the minimum CLI version for trusted
publishing). nodejs_22 ships npm 10.x and `npm publish` fails with
`ENEEDAUTH` because it can't speak the OIDC exchange.

## Release flow

Tag `v*` → `git push --tags`. The `.github/workflows/release.yml`
workflow runs the matrix `nix run .#publish-platform-binary -- <plat>`
for `linux-x64 / linux-arm64 / darwin-arm64` (no `darwin-x64` —
Apple Silicon only), then `nix run .#publish-meta` for the wrapper +
runtime. Six packages total go to the `@dialo` scope on npm.

Auth: trusted publishing via OIDC. **One-time npm-side setup** per
package (npm web UI → package settings → Trusted Publishers, or
`npm trusted-publisher add` with npm 11.5+):
- Type: GitHub Actions
- Repository: `dialohq/swell`
- Workflow file: `release.yml`
- Environment: (leave blank)

Repeat for `@dialo/swell`, `@dialo/swell-cli`, and the three platform
packages. Configure *before* the first tag — npm accepts the
OIDC-authenticated upload as the package's initial publish.
