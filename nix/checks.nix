# Tests and clippy. They run in the dev profile, like the `cargo test` and `cargo clippy` we run
# before committing, and share one dependency build.
{ lib, craneLib }:
let
  common = import ./common.nix { inherit lib; };
  # An empty profile makes crane run plain `cargo test` and `cargo clippy` (dev and test profiles).
  args = common.args // {
    CARGO_PROFILE = "";
    # Only the shared dependency build is worth keeping in the store and the cache.
    doInstallCargoArtifacts = false;
  };
  # Dependency artifacts: `cargo check` metadata for clippy, and the compiled dependencies and
  # dev-dependencies for the tests.
  cargoArtifacts = craneLib.buildDepsOnly (
    common.args
    // (common.depsArgs craneLib)
    // {
      pname = "shlv-checks";
      CARGO_PROFILE = "";
      buildPhaseCargoCommand = "cargoWithProfile check --locked --all-targets";
      checkPhaseCargoCommand = "cargoWithProfile test --locked --no-run";
    }
  );
in
{
  test = craneLib.cargoTest (
    args
    // {
      inherit cargoArtifacts;
      # The fetch tests run a local HTTP server on 127.0.0.1.
      __darwinAllowLocalNetworking = true;
    }
  );
  clippy = craneLib.cargoClippy (
    args
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- -D warnings";
    }
  );
}
