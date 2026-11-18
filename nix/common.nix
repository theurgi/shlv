# Arguments shared by the package (`package.nix`) and the checks (`checks.nix`).
{ lib }:
let
  fs = lib.fileset;
in
{
  args = {
    pname = "shlv";
    inherit ((lib.importTOML ../Cargo.toml).package) version;
    strictDeps = true;

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

    # nixpkgs' cargo setup hook exports LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which overrides rusqlite's
    # `bundled` feature. crane doesn't use that hook, but pin it anyway: the package must match what
    # `cargo test` and `bench/run.sh` measure (see tasks/completed/009-nix-package.md).
    env.LIBSQLITE3_SYS_USE_PKG_CONFIG = "0";
  };

  # `cargo` arguments for a feature list.
  featureArgs =
    features:
    "--locked"
    + lib.optionalString (features != [ ]) " --features ${lib.concatStringsSep "," features}";
}
