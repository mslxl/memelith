{
  description = "Rust app";

  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
      fenix,
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ fenix.overlays.default ];
        };
        toolchain = fenix.packages.${system}.combine (
          with fenix.packages.${system}.stable;
          [
            cargo
            rustc
            rust-src
            clippy
            rustfmt
          ]
        );
        buildInputs = [
          toolchain
          pkgs.pkg-config
          pkgs.diesel-cli
          pkgs.cargo-bundle
          pkgs.macdylibbundler
          pkgs.cargo-tauri
          pkgs.ffmpeg
        ];
      in
      {
        # Used by `nix develop`
        devShell = pkgs.mkShell {
          inherit buildInputs;

          # Specify the rust-src path (many editors rely on this)
          RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
          GNU_LIBICONV_PATH = "${pkgs.libiconvReal}/lib/libiconv.2.dylib";
        };
      }
    );
}
