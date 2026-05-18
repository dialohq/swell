{
  description = "swell — static type-checking for inline Postgres queries in TypeScript";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    # Linux only for now — the analyzer's bindgen path bakes in glibc-dev
    # headers, which doesn't make sense on Darwin. Widen this once
    # cross-platform is needed.
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        nativeBuildInputs = with pkgs; [
          pkg-config
          openssl
          postgresql
          libiconv
          # pg_query's build.rs uses bindgen, which needs libclang.
          llvmPackages.libclang
        ];

        buildInputs = with pkgs; [
          rustToolchain
          nodejs_20
          bun
          postgresql
          openssl
        ];

        # Header search path for pg_query's bindgen. Reused by every check
        # that touches the analyzer (which depends on pg_query).
        bindgenExtraClangArgs = builtins.toString [
          "-I${pkgs.glibc.dev}/include"
          "-I${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.lib.versions.major pkgs.llvmPackages.libclang.version}/include"
        ];

        commonEnv = {
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = bindgenExtraClangArgs;
        };

        # tsc-driven check wrapped as a derivation so it lands in
        # `checks.<system>` and runs under nix-fast-build. Uses bun for
        # the install + invocation — no `buildNpmPackage` indirection,
        # no `npmDepsHash` to keep in sync. `__noChroot = true` matches
        # the pattern used in dialo (the canonical bun-in-nix workload):
        # bun reaches the registry directly, frozen-lockfile makes the
        # result reproducible.
        mkTscCheck = { pname, tsc }: pkgs.stdenv.mkDerivation {
          inherit pname;
          version = "0.1.0";
          src = ./.;
          nativeBuildInputs = [ pkgs.bun pkgs.nodejs_20 ];
          buildPhase = ''
            runHook preBuild
            export HOME=$(mktemp -d)
            export BUN_INSTALL_CACHE_DIR=$(mktemp -d)
            bun install --frozen-lockfile
            patchShebangs node_modules
            ${tsc}
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            touch $out/ok
            runHook postInstall
          '';
          __noChroot = true;
        };

        runtimeTypecheck = mkTscCheck {
          pname = "swell-runtime-typecheck";
          tsc = "(cd packages/runtime && bun x tsc -p tsconfig.json --noEmit)";
        };

        # Exercises the q() overload + pg module augmentation end-to-end
        # against the example's `swell.generated.ts`. Builds the runtime
        # first so the workspace symlink resolves to compiled `.d.ts` /
        # `.js`, not bare source.
        exampleTypecheck = mkTscCheck {
          pname = "swell-example-basic-typecheck";
          tsc = ''
            (cd packages/runtime && bun x tsc -p tsconfig.json)
            (cd examples/basic && bun x tsc -p tsconfig.json --noEmit)
          '';
        };

        # Cargo build over the workspace, wrapped as a derivation. Deps
        # are vendored from Cargo.lock; the build is hermetic. Tests that
        # need a live Postgres run via `nix develop -c cargo test` (see
        # README) — they're outside the CI check because nix's sandbox
        # blocks network and the integration tests are intentionally
        # fail-loud about that.
        cargoBuild = pkgs.rustPlatform.buildRustPackage ({
          pname = "swell";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: _type:
              let p = baseNameOf (toString path); in
              !(builtins.elem p [ "target" "node_modules" "result" ".swell" ]);
          };
          cargoLock.lockFile = ./Cargo.lock;
          inherit nativeBuildInputs;
          buildInputs = buildInputs;
          # Don't run cargo tests in this derivation — they need a live
          # Postgres which the nix sandbox can't provide. Tests run via
          # `nix develop -c cargo test --workspace` in dev / a separate
          # workflow with a postgres service.
          doCheck = false;
        } // commonEnv);

        # Common env that the publish apps assume: rust + node + bun +
        # libclang + postgres headers. Same surface as devShells.default
        # but as a runtime PATH rather than an interactive shell.
        publishEnv = pkgs.symlinkJoin {
          name = "swell-publish-env";
          paths = [ rustToolchain pkgs.nodejs_22 pkgs.bun ] ++ nativeBuildInputs ++ buildInputs;
        };
      in
      {
        devShells.default = pkgs.mkShell ({
          inherit buildInputs nativeBuildInputs;

          shellHook = ''
            export PGDATA="$PWD/.postgres-data"
            export PGHOST="$PWD/.postgres-sock"
            echo "swell dev shell"
            echo "  rustc: $(rustc --version)"
            echo "  node:  $(node --version)"
            echo "  bun:   $(bun --version)"
            echo "  pg:    $(postgres --version)"
          '';
        } // commonEnv);

        # `nix develop .#publish` — release-only shell.
        #
        # `npm` is intentionally absent from the default devshell so
        # day-to-day work stays bun-driven (no package-manager
        # bait-and-switch). The publish path lives here because
        # `bun publish` doesn't yet exchange the GitHub Actions OIDC
        # id-token for an npm trusted-publishing access token —
        # `npm publish --provenance` does. The release.yml workflow uses
        # the same `npm` binary; this shell lets a human reproduce the
        # publish step locally (e.g. for dry-runs against a local
        # registry).
        devShells.publish = pkgs.mkShell {
          buildInputs = [ pkgs.nodejs_22 ];
          shellHook = ''
            echo "swell publish shell"
            echo "  npm: $(npm --version) (trusted publishing needs >= 11.5)"
            if [ "$(npm --version | cut -d. -f1)" -lt 11 ]; then
              echo "  ↳ run \`npm install -g npm@latest\` to bump"
            fi
          '';
        };

        # `nix run .#publish` — dry-run the publish flow over all
        # locally-checked-out packages. No auth required; catches
        # packaging mistakes (missing files, bad version, wrong scope)
        # before a tag goes out. For real publishing, push a v* tag —
        # release.yml handles it via GitHub Actions OIDC trusted
        # publishing.
        apps.publish = {
          type = "app";
          program = "${pkgs.writeShellScript "swell-publish-dry-run" ''
            set -euo pipefail
            export PATH="${publishEnv}/bin:$PATH"
            echo "swell publish dry-run — npm $(npm --version)"
            echo ""
            bun install --frozen-lockfile
            bun run build:runtime
            for pkg in packages/runtime packages/swell-cli; do
              echo ""
              echo "=== $pkg ==="
              (cd "$pkg" && npm publish --dry-run --access public)
            done
            echo ""
            echo "(Platform binaries — @dialo/swell-cli-{linux,darwin}-{x64,arm64} —"
            echo " are only built on the release matrix; not dry-run-able locally.)"
          ''}";
        };

        # `nix run .#publish-platform-binary -- <platform>`
        # Native-build the CLI on the host, generate the per-platform
        # `package.json`, publish via npm trusted publishing. Called
        # once per matrix runner in `.github/workflows/release.yml`.
        # Reads `VERSION` from env (set by the workflow from the tag).
        apps.publish-platform-binary = {
          type = "app";
          program = "${pkgs.writeShellScript "swell-publish-platform-binary" ''
            set -euo pipefail
            export PATH="${publishEnv}/bin:$PATH"
            export ${pkgs.lib.concatStringsSep " " (
              pkgs.lib.mapAttrsToList (k: v: "${k}=${v}") commonEnv
            )}

            PLATFORM="''${1:?usage: nix run .#publish-platform-binary -- <linux-x64|linux-arm64|darwin-x64|darwin-arm64>}"
            VERSION="''${VERSION:?VERSION env var required (no v prefix)}"
            # `linux-x64` → OS=linux, CPU=x64
            OS="''${PLATFORM%-*}"
            CPU="''${PLATFORM##*-}"

            echo "building swell-cli for $PLATFORM (v$VERSION) — npm $(npm --version)"
            cargo build --release -p swell-cli

            DIR="dist-platform/$PLATFORM"
            mkdir -p "$DIR/bin"
            cp target/release/swell "$DIR/bin/swell"
            cat > "$DIR/package.json" <<EOF
            {
              "name": "@dialo/swell-cli-$PLATFORM",
              "version": "$VERSION",
              "description": "Native swell-cli binary for $OS/$CPU. Installed automatically by @dialo/swell-cli.",
              "license": "MIT OR Apache-2.0",
              "publishConfig": { "access": "public" },
              "os": ["$OS"],
              "cpu": ["$CPU"],
              "bin": { "swell": "bin/swell" },
              "files": ["bin"],
              "repository": {
                "type": "git",
                "url": "git+https://github.com/dialohq/swell.git"
              }
            }
            EOF
            (cd "$DIR" && npm publish --access public --provenance)
          ''}";
        };

        # `nix run .#publish-meta`
        # Publish the wrapper + runtime packages — runs after the matrix
        # has published the four platform binaries. Reads `VERSION`
        # from env. The wrapper's `optionalDependencies` get rewritten
        # in-place to pin all four platform packages to the same version.
        apps.publish-meta = {
          type = "app";
          program = "${pkgs.writeShellScript "swell-publish-meta" ''
            set -euo pipefail
            export PATH="${publishEnv}/bin:$PATH"

            VERSION="''${VERSION:?VERSION env var required (no v prefix)}"

            echo "publishing @dialo/swell-cli (wrapper) + @dialo/swell (runtime) — npm $(npm --version)"

            # Pin wrapper version + every platform optionalDependency.
            bun --print '
              const fs = require("node:fs");
              const path = "packages/swell-cli/package.json";
              const p = JSON.parse(fs.readFileSync(path, "utf8"));
              p.version = process.env.VERSION;
              for (const k of Object.keys(p.optionalDependencies ?? {})) {
                p.optionalDependencies[k] = process.env.VERSION;
              }
              fs.writeFileSync(path, JSON.stringify(p, null, 2) + "\n");
            '
            (cd packages/swell-cli && npm publish --access public --provenance)

            # Pin runtime version, build dist/, publish.
            bun --print '
              const fs = require("node:fs");
              const path = "packages/runtime/package.json";
              const p = JSON.parse(fs.readFileSync(path, "utf8"));
              p.version = process.env.VERSION;
              fs.writeFileSync(path, JSON.stringify(p, null, 2) + "\n");
            '
            bun install --frozen-lockfile
            bun run build:runtime
            (cd packages/runtime && npm publish --access public --provenance)
          ''}";
        };

        # `swell-cli` binary, exposed as a flake package so Nix users
        # skip npm entirely:
        #   nix run github:dialohq/swell#swell-cli -- gen
        #   nix shell github:dialohq/swell -c swell gen
        packages.swell-cli = cargoBuild;
        packages.default = cargoBuild;

        # nix-fast-build target. q3 layers the example-typecheck check
        # on top of the runtime + cargo checks the scaffold (q0) wired.
        checks = {
          runtime-typecheck = runtimeTypecheck;
          example-typecheck = exampleTypecheck;
          cargo-build = cargoBuild;
        };
      });
}
