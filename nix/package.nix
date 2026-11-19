# The shlv package, built with crane (task 049). The dependencies are their own derivation
# (`passthru.cargoArtifacts`), so a source change rebuilds only shlv, and the binary cache keeps
# the dependency build until `Cargo.lock` changes.
#
# Tests and clippy don't run here: they're flake checks (`nix/checks.nix`), in the dev profile.
# Running them in release would link every test binary again with fat LTO, which more than doubled
# the build.
{ lib, craneLib }:
let
  inherit (import ./common.nix { inherit lib; }) args depsArgs;
  cargoArtifacts = craneLib.buildDepsOnly (
    args
    // (depsArgs craneLib)
    // {
      # The package needs only the compiled dependencies, not `cargo check`'s metadata or the
      # dev-dependencies.
      cargoCheckCommand = "true";
      doCheck = false;
    }
  );
in
craneLib.buildPackage (
  args
  // {
    inherit cargoArtifacts;
    doCheck = false;
    passthru = { inherit cargoArtifacts; };

    meta = {
      description = "A fast, resource-efficient bookmarking app";
      homepage = "https://github.com/theurgi/shlv";
      license = lib.licenses.mit;
      mainProgram = "shlv";
    };
  }
)
