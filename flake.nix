{
  description = "shlv: a fast, resource-efficient bookmarking app";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAll = f: lib.genAttrs systems (s: f nixpkgs.legacyPackages.${s});

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
      overlays.default = final: _prev: { shlv = final.callPackage ./nix/package.nix { }; };

      packages = forAll (pkgs: rec {
        shlv = pkgs.callPackage ./nix/package.nix { };
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
          shlv = self.packages.${pkgs.stdenv.hostPlatform.system}.shlv;
          # A check that runs `script` over the lint source and succeeds if it exits 0.
          lint =
            name: inputs: script:
            pkgs.runCommand "shlv-${name}" { nativeBuildInputs = inputs; } ''
              cd ${lintSrc}
              ${script}
              touch $out
            '';
        in
        {
          inherit shlv;
          # Reuses the package's vendored dependencies, so nothing is fetched twice.
          clippy = shlv.overrideAttrs (old: {
            pname = "shlv-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --offline --all-targets -- -D warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = "touch $out";
          });
          rustfmt = lint "rustfmt" [
            pkgs.cargo
            pkgs.rustfmt
          ] "cargo fmt --check";
          oxfmt = lint "oxfmt" [ pkgs.oxfmt ] "oxfmt --check .";
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
