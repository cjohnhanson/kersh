{
  description = "kersh — a declarative agent runner";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ...
    }:
    let
      forEachSystem =
        f:
        nixpkgs.lib.genAttrs
          [
            "aarch64-darwin"
            "x86_64-linux"
            "aarch64-linux"
            "x86_64-darwin"
          ]
          (
            system:
            f {
              pkgs = import nixpkgs {
                inherit system;
                overlays = [ rust-overlay.overlays.default ];
              };
              inherit system;
            }
          );
    in
    {
      packages = forEachSystem (
        { pkgs, system }:
        let
          toolchain = pkgs.rust-bin.stable.latest.default;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };

          # cargo build needs the sources, the manifest, the lock, and the
          # bundled docs (`include_str!` reads docs/kersh.md). It does not
          # need the build target, which would bloat the store copy.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (pkgs.lib.cleanSourceFilter path type) && (builtins.match ".*/target(/.*)?" path == null);
          };

          kersh = rustPlatform.buildRustPackage {
            pname = "kersh";
            version = "0.5.0";
            inherit src;
            cargoLock = {
              lockFile = ./Cargo.lock;
              # The one git dependency needs its checkout hash. crates.io
              # dependencies are vendored from the lock without one.
              outputHashes = {
                "rig-claude-code-0.1.0" = "sha256-fCt4VI5grAPMoxFn1iSYpaWtpRUqtI2aaww+cI5Wtyo=";
              };
            };
            # The tests run in kersh's own CI. The nix build produces the
            # binary; its unit tests spawn subprocesses and its end-to-end
            # tests need a model, neither of which a sandbox build wants.
            doCheck = false;
            buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];
          };
        in
        {
          default = kersh;
          inherit kersh;
        }
      );
    };
}
