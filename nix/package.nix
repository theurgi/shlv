{
  lib,
  rustPlatform,
  # Cargo features to build with; `shlv-full` passes all of them.
  features ? [ ],
  pname ? "shlv",
}:
let
  fs = lib.fileset;
in
rustPlatform.buildRustPackage {
  inherit pname;
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
  buildFeatures = features;

  # The fetch tests run a local HTTP server on 127.0.0.1.
  __darwinAllowLocalNetworking = true;

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
