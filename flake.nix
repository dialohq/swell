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
          npmDepsHash = "sha256-rWPHy6BXEMynTefJ1raAIeXWT2tJ4coQpuN9AmrXbwQ=";
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
        };
      });
}
