{
  lib,
  rustPlatform,
  pkg-config,
  sqlite,
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

  # nixpkgs' cargo setup hook sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which overrides rusqlite's
  # `bundled` feature and links the system SQLite instead (nixpkgs' sqlite has FTS5).
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ sqlite ];

  meta = {
    description = "A fast, resource-efficient bookmarking app";
    homepage = "https://github.com/theurgi/shlv";
    license = lib.licenses.mit;
    mainProgram = "shlv";
  };
}
