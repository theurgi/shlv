# Install

## Nix

With Nix:

```sh
nix run github:theurgi/shlv -- --help
nix profile add github:theurgi/shlv
```

The flake also provides `overlays.default`, which adds `pkgs.shlv`.

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

## Optional features

The default build is offline. Network features are Cargo features, off by default:

| Feature | Adds                                                                                |
| ------- | ----------------------------------------------------------------------------------- |
| `fetch` | `add --fetch` and `shlv fetch`: page titles from the web                            |
| `ai`    | AI summaries and tag suggestions through a provider you configure (implies `fetch`) |
| `model` | An in-binary tag classifier that learns from your tags; offline, no dependencies    |

The flake's `shlv-full` package has every feature (`nix run github:theurgi/shlv#shlv-full`). With
Cargo, pass `--features fetch,ai,model` (or `--all-features`).
