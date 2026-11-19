# Install

## Nix

With Nix:

```sh
nix run github:theurgi/shlv -- --help
nix profile add github:theurgi/shlv
```

The flake also provides `overlays.default`, which adds `pkgs.shlv`.

CI pushes its builds to the `shlv` Cachix cache, so you can download shlv instead of compiling it.
Nix offers to use the cache when you run the flake. On NixOS, add it to your system configuration
instead:

```nix
nix.settings = {
  substituters = [ "https://shlv.cachix.org" ];
  trusted-public-keys = [ "shlv.cachix.org-1:6n2xlDaLnUZNrzzJnnJ/31eoqdlbDKuDG83yGPfS3zo=" ];
};
```

The cache only matches builds that use shlv's own `flake.lock`. If your flake sets
`inputs.shlv.inputs.nixpkgs.follows`, every hash changes and Nix compiles shlv locally.

With [Home Manager](https://github.com/nix-community/home-manager), add `shlv` as a flake input,
then import its module. `settings` is written to `~/.config/shlv/config.toml` (see
[Config](config.md)):

```nix
{ shlv, ... }:
{
  imports = [ shlv.homeManagerModules.default ];

  programs.shlv = {
    enable = true;
    settings = {
      strict_tags = true;
      tags.canonical = {
        javascript = [ "js" "ecmascript" ];
        rust = [ ];
      };
      tags.rules."docs.rs" = [ "rust" ];
    };
  };
}
```

Don't put secrets in `settings`, since it ends up in the Nix store.

## Cargo

```sh
cargo install --git https://github.com/theurgi/shlv
```
