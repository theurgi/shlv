{
  lib,
  rustPlatform,
}:
let
  fs = lib.fileset;
in
rustPlatform.buildRustPackage {
  pname = "shlv";
  inherit ((lib.importTOML ../Cargo.toml).package) version;

  src = fs.toSource {
    root = ../.;
    fileset = fs.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../tests
      ../migrations
      ../examples
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # nixpkgs' cargo setup hook exports LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which overrides rusqlite's
  # `bundled` feature. Keep the bundled SQLite so the package matches what `cargo test` and
  # `bench/run.sh` measure (see tasks/completed/009-nix-package.md).
  preBuild = "export LIBSQLITE3_SYS_USE_PKG_CONFIG=0";

  meta = {
    description = "A fast, resource-efficient bookmarking app";
    homepage = "https://github.com/theurgi/shlv";
    license = lib.licenses.mit;
    mainProgram = "shlv";
  };
}
