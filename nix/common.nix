# Arguments shared by the package (`package.nix`) and the checks (`checks.nix`).
{ lib }:
let
  fs = lib.fileset;
  inherit ((lib.importTOML ../Cargo.toml).package) version;
  sources = [
    ../Cargo.toml
    ../src
    ../tests
    ../migrations
    ../examples
  ];
  source =
    files:
    fs.toSource {
      root = ../.;
      fileset = fs.unions files;
    };
in
{
  args = {
    pname = "shlv";
    inherit version;
    strictDeps = true;
    cargoExtraArgs = "--locked";
    src = source (sources ++ [ ../Cargo.lock ]);

    # nixpkgs' cargo setup hook exports LIBSQLITE3_SYS_USE_PKG_CONFIG=1, which overrides rusqlite's
    # `bundled` feature. crane doesn't use that hook, but pin it anyway: the package must match what
    # `cargo test` and `bench/run.sh` measure (see tasks/completed/009-nix-package.md).
    env.LIBSQLITE3_SYS_USE_PKG_CONFIG = "0";
  };

  # Arguments for every `buildDepsOnly`, so a version bump keeps the dependency builds (task 050).
  # crane's dummy source would carry shlv's version from `Cargo.toml` and `Cargo.lock`, and its
  # derivation hash depends on those inputs, not just on its output. So the version is removed at
  # evaluation time: the cleaned `Cargo.toml` drops it (Cargo then uses 0.0.0), and `Cargo.lock`
  # comes from a normalized copy instead of the source. The compiled dependencies don't depend on it.
  depsArgs =
    craneLib:
    let
      cargoLock = builtins.toFile "Cargo.lock" (
        let
          lock = builtins.readFile ../Cargo.lock;
          entry = v: "name = \"shlv\"\nversion = \"${v}\"\n";
        in
        assert lib.assertMsg (lib.hasInfix (entry version) lock) "Cargo.lock has no shlv ${version} entry";
        builtins.replaceStrings [ (entry version) ] [ (entry "0.0.0") ] lock
      );
    in
    {
      version = "0.0.0";
      # `dummySrc` replaces `src`. Vendoring reads the real tree: crawling the dummy source at
      # evaluation time would be import-from-derivation.
      src = null;
      inherit cargoLock;
      cargoVendorDir = craneLib.vendorCargoDeps {
        src = source sources;
        inherit cargoLock;
      };
      dummySrc = craneLib.mkDummySrc {
        src = source sources;
        inherit cargoLock;
        cleanCargoTomlFilter =
          path: craneLib.filters.cargoTomlDefault path && lib.concatStringsSep "." path != "package.version";
      };
    };

}
