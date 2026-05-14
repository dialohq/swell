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

        # `tsc --noEmit` over packages/runtime, wrapped as a derivation so
        # it lands in `checks.<system>` and runs under nix-fast-build. Deps
        # come from the workspace's npm lockfile committed at the repo
        # root; buildNpmPackage materialises them in a sandboxed
        # node_modules (no live npm-registry access).
        runtimeTypecheck = pkgs.buildNpmPackage {
          pname = "swell-runtime-typecheck";
          version = "0.1.0";
          src = ./.;
          npmDepsHash = "sha256-U6wTTLbKnI89vrMUBolSLGwnQZIn133jpI3dYhzNWMk=";
          dontNpmBuild = true;
          # Run tsc directly against the runtime sources. No `dist/` is
          # emitted (`--noEmit`); a marker file is the derivation output.
          installPhase = ''
            runHook preInstall
            cd packages/runtime
            npx tsc -p tsconfig.json --noEmit
            mkdir -p $out
            touch $out/ok
            runHook postInstall
          '';
        };

        # `tsc --noEmit` over examples/basic. Exercises the q() overload
        # + pg module augmentation end-to-end against the example's
        # `swell.generated.ts`, so a regression in either lands here
        # before it lands in user code.
        exampleTypecheck = pkgs.buildNpmPackage {
          pname = "swell-example-basic-typecheck";
          version = "0.0.0";
          src = ./.;
          npmDepsHash = "sha256-U6wTTLbKnI89vrMUBolSLGwnQZIn133jpI3dYhzNWMk=";
          dontNpmBuild = true;
          installPhase = ''
            runHook preInstall
            # Build the runtime first so the workspace symlink resolves
            # to compiled `.d.ts` / `.js`, not bare source.
            (cd packages/runtime && npx tsc -p tsconfig.json)
            (cd examples/basic && npx tsc -p tsconfig.json --noEmit)
            mkdir -p $out
            touch $out/ok
            runHook postInstall
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

        checks = {
          runtime-typecheck = runtimeTypecheck;
          example-typecheck = exampleTypecheck;
          cargo-build = cargoBuild;
        };
      });
}
