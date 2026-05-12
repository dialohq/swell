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
      in
      {
        devShells.default = pkgs.mkShell {
          inherit buildInputs nativeBuildInputs;

          # bindgen looks at LIBCLANG_PATH; without this, pg_query's build fails.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

          # bindgen also needs to find system headers (sys/types.h, stddef.h etc.).
          # On Nix we have to point it at glibc-dev and clang's builtin headers
          # explicitly — there's no system /usr/include.
          BINDGEN_EXTRA_CLANG_ARGS = builtins.toString ([
            "-I${pkgs.glibc.dev}/include"
            "-I${pkgs.llvmPackages.libclang.lib}/lib/clang/${pkgs.lib.versions.major pkgs.llvmPackages.libclang.version}/include"
          ]);

          shellHook = ''
            export PGDATA="$PWD/.postgres-data"
            export PGHOST="$PWD/.postgres-sock"
            echo "swell dev shell"
            echo "  rustc: $(rustc --version)"
            echo "  node:  $(node --version)"
            echo "  bun:   $(bun --version)"
            echo "  pg:    $(postgres --version)"
          '';
        };

        # packages.default = pkgs.rustPlatform.buildRustPackage { ... }
        # Re-enable once Cargo.lock is committed — see flake.nix history.
      });
}
