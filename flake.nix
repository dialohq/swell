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
    flake-utils.lib.eachDefaultSystem (system:
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

        # Per-branch checks live here. They're picked up by `nix flake check`
        # and `nix-fast-build .#checks` — CI just runs the latter. Adding a
        # new build/test step means adding a derivation here; nix's
        # content-addressed cache means each one only re-runs when its
        # actual inputs change.
        #
        # Branch evolution:
        #   - main:        no checks (scaffolding only)
        #   - feat/runtime: adds `runtime-typecheck`
        #   - feat/analyzer: adds `cargo-build` + `cargo-test`
        #   - feat/codegen: inherits both; same checks scale to more crates
        checks = { };
      });
}
