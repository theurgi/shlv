# Tests and clippy for the default features and for all of them. They run in the dev profile, like
# the `cargo test` and `cargo clippy` we run before committing, and share one dependency build.
{
  lib,
  craneLib,
  # Every optional Cargo feature.
  allFeatures,
}:
let
  common = import ./common.nix { inherit lib; };
  default = common.featureArgs [ ];
  full = common.featureArgs allFeatures;
  # An empty profile makes crane run plain `cargo test` and `cargo clippy` (dev and test profiles).
  args = common.args // {
    CARGO_PROFILE = "";
    # Only the shared dependency build is worth keeping in the store and the cache.
    doInstallCargoArtifacts = false;
  };
  # Dependency artifacts for both feature sets: `cargo check` metadata for clippy, and the compiled
  # dependencies and dev-dependencies for the tests.
  cargoArtifacts = craneLib.buildDepsOnly (
    common.args
    // {
      pname = "shlv-checks";
      CARGO_PROFILE = "";
      buildPhaseCargoCommand = ''
        cargoWithProfile check ${default} --all-targets
        cargoWithProfile check ${full} --all-targets
      '';
      checkPhaseCargoCommand = ''
        cargoWithProfile test ${default} --no-run
        cargoWithProfile test ${full} --no-run
      '';
    }
  );
  test =
    pname: cargoExtraArgs:
    craneLib.cargoTest (
      args
      // {
        inherit pname cargoArtifacts cargoExtraArgs;
        # The fetch tests run a local HTTP server on 127.0.0.1.
        __darwinAllowLocalNetworking = true;
      }
    );
  clippy =
    pname: cargoExtraArgs:
    craneLib.cargoClippy (
      args
      // {
        inherit pname cargoArtifacts cargoExtraArgs;
        cargoClippyExtraArgs = "--all-targets -- -D warnings";
      }
    );
in
{
  test = test "shlv" default;
  test-full = test "shlv-full" full;
  clippy = clippy "shlv" default;
  clippy-full = clippy "shlv-full" full;
}
