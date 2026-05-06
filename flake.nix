{
  description = "Development Shell for Docker LVM Plugin Repo";

  inputs = {
    nixpkgs.url = "nixpkgs/nixos-25.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems f;
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.clang
              pkgs.llvmPackages.libclang.lib
            ];

            buildInputs = [
              rustToolchain
              pkgs.util-linux     # Essential for libblkid
              pkgs.glibc.dev      # Provides standard C headers
              pkgs.xfsprogs
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            # Use -isystem to treat these as system headers and avoid warnings
            BINDGEN_EXTRA_CLANG_ARGS =
              let
                # Use the major version (e.g. "18") instead of the full version (e.g. "18.1.8")
                libclangVersion = pkgs.lib.versions.major pkgs.llvmPackages.libclang.version;
              in
              builtins.concatStringsSep " " [
                "-isystem ${pkgs.glibc.dev}/include"
                "-isystem ${pkgs.llvmPackages.libclang.lib}/lib/clang/${libclangVersion}/include"
              ];
          };
        }
      );
    };
}
