{
  description = "yet another mcp obsidian server, for obsidian livesync via couchdb";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {flake-parts, crane, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];

      perSystem = {
        config,
        self',
        inputs',
        pkgs,
        system,
        ...
      }: let
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = ["rust-src" "rust-analyzer"];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # Common source filtering
        src = craneLib.cleanCargoSource ./.;

        # Common arguments for all builds
        commonArgs = {
          inherit src;
          strictDeps = true;

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            openssl
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.Security
            pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
          ];
        };

        # Build just the cargo dependencies for caching
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Build the actual package
        yamos = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });
      in {
        _module.args.pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [(import inputs.rust-overlay)];
        };

        checks = {
          inherit yamos;

          # Run clippy
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          # Check formatting
          fmt = craneLib.cargoFmt {
            inherit src;
          };
        };

        packages = {
          default = yamos;
          inherit yamos;
        };

        devShells.default = craneLib.devShell {
          checks = self'.checks;

          packages = with pkgs; [
            cargo-update
            cargo-edit
            curl
            jq
          ];

          RUST_LOG = "yamos=debug";

          shellHook = ''
            echo "yamos dev shell"
            echo ""
            echo "Quick start:"
            echo "  1. cp .env.example .env"
            echo "  2. Edit .env with your CouchDB credentials"
            echo "  3. cargo run -- --transport sse"
            echo ""
            echo "For production build:"
            echo "  nix build"
          '';
        };
      };
    };
}
