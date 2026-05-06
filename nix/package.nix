{
  lib,
  rustPlatform,
  pkg-config,
  clang,
  llvmPackages,
  util-linux,
  glibc,
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;

  src = lib.cleanSource ./..;

  cargoLock = {
    lockFile = ../Cargo.lock;
    allowBuiltinFetchGit = true;
  };

  nativeBuildInputs = [
    pkg-config
    clang
    llvmPackages.libclang.lib
  ];

  buildInputs = [
    util-linux # libblkid
  ];

  LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";

  BINDGEN_EXTRA_CLANG_ARGS =
    let
      libclangVersion = lib.versions.major llvmPackages.libclang.version;
    in
    builtins.concatStringsSep " " [
      "-isystem ${glibc.dev}/include"
      "-isystem ${llvmPackages.libclang.lib}/lib/clang/${libclangVersion}/include"
    ];

  meta = {
    description = cargoToml.package.description;
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = cargoToml.package.name;
  };
}
