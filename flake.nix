{
  description = "shlv: a fast, resource-efficient bookmarking app";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Builds the dependencies as their own derivation, so they're cached apart from shlv (task 049).
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      ...
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAll = f: lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});
      allFeatures = lib.remove "default" (
        builtins.attrNames ((lib.importTOML ./Cargo.toml).features or { })
      );

      # Everything tracked, minus build output and local data. Used by the lint checks.
      lintSrc = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.difference ./. (
          lib.fileset.unions (
            map lib.fileset.maybeMissing [
              ./target
              ./.direnv
              ./bench/data
              ./result
            ]
          )
        );
      };
    in
    {
      overlays.default = final: _prev: {
        shlv = final.callPackage ./nix/package.nix { craneLib = crane.mkLib final; };
      };

      homeManagerModules.default = import ./nix/hm-module.nix { inherit self; };

      packages = forAll (pkgs: rec {
        shlv = pkgs.callPackage ./nix/package.nix { craneLib = crane.mkLib pkgs; };
        # Every optional Cargo feature (fetch, and later ai and model).
        shlv-full = shlv.override {
          pname = "shlv-full";
          features = allFeatures;
        };
        default = shlv;
      });

      apps = forAll (pkgs: {
        default = {
          type = "app";
          program = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.shlv;
          meta.description = "Run shlv";
        };
      });

      checks = forAll (
        pkgs:
        let
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) shlv shlv-full;
          # A check that runs `script` over the lint source and succeeds if it exits 0.
          lint =
            name: inputs: script:
            pkgs.runCommand "shlv-${name}" { nativeBuildInputs = inputs; } ''
              cd ${lintSrc}
              ${script}
              touch $out
            '';
        in
        # Tests and clippy, for the default features and for all of them.
        import ./nix/checks.nix {
          inherit lib allFeatures;
          craneLib = crane.mkLib pkgs;
        }
        // {
          # The release builds.
          inherit shlv shlv-full;
          rustfmt = lint "rustfmt" [
            pkgs.cargo
            pkgs.rustfmt
          ] "cargo fmt --check";
          oxfmt = lint "oxfmt" [ pkgs.oxfmt ] "oxfmt --check .";
          hm-module = import ./nix/hm-module-test.nix {
            inherit lib pkgs shlv-full;
            module = self.homeManagerModules.default;
          };
          nixfmt = lint "nixfmt" [ pkgs.nixfmt ] "nixfmt --check flake.nix nix/*.nix";
        }
      );

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            oxfmt
            nixfmt
            hyperfine
            sqlite
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
