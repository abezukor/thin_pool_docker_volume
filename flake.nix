{
  description = "Docker volume plugin backed by an LVM thin pool";

  inputs = {
    nixpkgs.url = "nixpkgs/nixos-25.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, rust-overlay, ... }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs supportedSystems f;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable."1.95.0".minimal;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        {
          thin-pool-docker-volume = pkgs.callPackage ./nix/package.nix { inherit rustPlatform; };
          default = self.packages.${system}.thin-pool-docker-volume;
        }
      );

      nixosModules.default = import ./nix/module.nix self;

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
              pkgs.util-linux
              pkgs.glibc.dev
              pkgs.xfsprogs
            ];

            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            BINDGEN_EXTRA_CLANG_ARGS =
              let
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
