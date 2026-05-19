{
  description = "swell — static type-checking for inline Postgres queries in TypeScript";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # crane gives us `buildDepsOnly` — a separate derivation that compiles
    # the Cargo.lock dependency set once and caches its `target/` between
    # subsequent `buildPackage` runs. A one-line swell-source change goes
    # from ~10 min (rebuild all 300+ deps with rustPlatform.buildRustPackage)
    # down to ~30s (compile just the workspace crates).
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachSystem [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        isDarwin = pkgs.stdenv.isDarwin;
        isAarch64 = pkgs.stdenv.isAarch64;

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        muslTarget =
          if isAarch64 then "aarch64-unknown-linux-musl"
          else "x86_64-unknown-linux-musl";

        rustMuslToolchain = rustToolchain.override {
          targets = [ muslTarget ];
        };

        muslCc = "${pkgs.pkgsMusl.stdenv.cc}/bin/cc";
        muslTargetEnv = builtins.replaceStrings ["-"] ["_"] muslTarget;

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
        # that touches the analyzer (which depends on pg_query). On Darwin
        # the SDK provides the system headers — only Linux needs the
        # glibc-dev path bolted on.
        bindgenExtraClangArgs = builtins.toString (
          pkgs.lib.optional (!isDarwin) "-I${pkgs.glibc.dev}/include" ++ [
            "-I${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.lib.versions.major pkgs.llvmPackages.libclang.version}/include"
          ]
        );

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

        # Cargo build over the workspace via crane. Two derivations:
        #   1. `cargoArtifacts` — `buildDepsOnly` compiles every Cargo.lock
        #      dependency with dummy workspace crates substituted in. Cached
        #      on the lockfile; survives any source-only change.
        #   2. `cargoBuild` — `buildPackage` reuses those artifacts and only
        #      recompiles the swell-* workspace crates when source changes.
        # Tests that need a live Postgres run via `nix develop -c cargo test`
        # — they're outside the nix sandbox because it blocks network and
        # the integration tests are intentionally fail-loud about that.
        cargoSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: _type:
            let p = baseNameOf (toString path); in
            !(builtins.elem p [ "target" "node_modules" "result" ".swell" ]);
          name = "swell-source";
        };

        cargoCommonArgs = {
          src = cargoSrc;
          strictDeps = true;
          inherit nativeBuildInputs buildInputs;
          # Don't run cargo tests in this derivation — they need a live
          # Postgres which the nix sandbox can't provide.
          doCheck = false;
        } // commonEnv;

        cargoArtifacts = craneLib.buildDepsOnly (cargoCommonArgs // {
          pname = "swell-deps";
          version = "0.1.0";
        });

        cargoBuild = craneLib.buildPackage (cargoCommonArgs // {
          pname = "swell";
          version = "0.1.0";
          inherit cargoArtifacts;
        });

        # Common env that the publish apps assume: rust + node + bun +
        # libclang + postgres headers. Same surface as devShells.default
        # but as a runtime PATH rather than an interactive shell.
        publishEnv = pkgs.symlinkJoin {
          name = "swell-publish-env";
          paths = [ rustToolchain pkgs.nodejs_24 pkgs.bun ] ++ nativeBuildInputs ++ buildInputs;
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

        # `nix develop .#release` — Linux targets musl (static binary,
        # runs anywhere); darwin uses the native toolchain.
        devShells.release = pkgs.mkShell (
          (if isDarwin then {
            buildInputs = [ rustToolchain ] ++ nativeBuildInputs;
          } else {
            buildInputs = [ rustMuslToolchain pkgs.pkgsMusl.stdenv.cc ] ++ nativeBuildInputs;
            shellHook = ''
              export CARGO_BUILD_TARGET=${muslTarget}
              export CARGO_TARGET_${pkgs.lib.toUpper muslTargetEnv}_LINKER=${muslCc}
              export CC_${pkgs.lib.toLower muslTargetEnv}=${muslCc}
            '';
          }) // commonEnv
        );

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
          buildInputs = [ pkgs.nodejs_24 ];
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

        # `nix run .#publish-platform-binary -- <platform>` packages a
        # pre-built `swell` binary (see `devShells.release`) into the
        # per-platform npm tarball and publishes via OIDC trusted
        # publishing. Reads `VERSION` from env.
        apps.publish-platform-binary = {
          type = "app";
          program = "${pkgs.writeShellScript "swell-publish-platform-binary" ''
            set -euo pipefail
            export PATH="${pkgs.nodejs_24}/bin:$PATH"

            PLATFORM="''${1:?usage: nix run .#publish-platform-binary -- <linux-x64|linux-arm64|darwin-arm64>}"
            VERSION="''${VERSION:?VERSION env var required (no v prefix)}"

            case "$PLATFORM" in
              linux-x64)    DEFAULT_BIN="target/x86_64-unknown-linux-musl/release/swell" ;;
              linux-arm64)  DEFAULT_BIN="target/aarch64-unknown-linux-musl/release/swell" ;;
              darwin-arm64) DEFAULT_BIN="target/release/swell" ;;
              *) echo "unknown platform $PLATFORM" >&2; exit 1 ;;
            esac
            BINARY="''${BINARY:-$DEFAULT_BIN}"
            # `linux-x64` → OS=linux, CPU=x64
            OS="''${PLATFORM%-*}"
            CPU="''${PLATFORM##*-}"
            # Prerelease versions (`0.0.0-foo`) must publish under a
            # non-`latest` dist-tag — npm 11+ refuses otherwise.
            DIST_TAG="latest"
            case "$VERSION" in *-*) DIST_TAG="next" ;; esac

            echo "packaging swell-cli for $PLATFORM (v$VERSION, --tag $DIST_TAG) — npm $(npm --version)"
            [ -x "$BINARY" ] || { echo "missing binary at $BINARY — did the workflow's cargo step run?" >&2; exit 1; }

            DIR="dist-platform/$PLATFORM"
            mkdir -p "$DIR/bin"
            cp "$BINARY" "$DIR/bin/swell"
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
            (cd "$DIR" && npm publish --access public --provenance --tag "$DIST_TAG")
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
            DIST_TAG="latest"
            case "$VERSION" in *-*) DIST_TAG="next" ;; esac

            echo "publishing @dialo/swell-cli (wrapper) + @dialo/swell (runtime), v$VERSION --tag $DIST_TAG — npm $(npm --version)"

            # Install workspace deps + build runtime BEFORE any version
            # mutation — `--frozen-lockfile` only matches if package.json
            # files still reflect the lockfile's resolved state. Once we
            # rewrite versions below, the lockfile would no longer match
            # and the install would refuse.
            bun install --frozen-lockfile
            bun run build:runtime

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
            (cd packages/swell-cli && npm publish --access public --provenance --tag "$DIST_TAG")

            # Pin runtime version + publish (dist/ already built above).
            bun --print '
              const fs = require("node:fs");
              const path = "packages/runtime/package.json";
              const p = JSON.parse(fs.readFileSync(path, "utf8"));
              p.version = process.env.VERSION;
              fs.writeFileSync(path, JSON.stringify(p, null, 2) + "\n");
            '
            (cd packages/runtime && npm publish --access public --provenance --tag "$DIST_TAG")
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
