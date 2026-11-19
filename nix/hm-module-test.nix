# Flake check for the home-manager module. Evaluates it with `lib.evalModules` and stubs for the two
# home-manager options it sets, then has shlv load the rendered config, pin the tag list, and add to it.
{
  lib,
  pkgs,
  module,
  shlv,
}:
let
  # Only compared at evaluation time, never built.
  chosen = pkgs.writeShellScriptBin "shlv" "";
  stubs = {
    options.home.packages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
    };
    options.xdg.configFile = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule { options.source = lib.mkOption { type = lib.types.path; }; }
      );
      default = { };
    };
  };

  eval =
    cfg:
    (lib.evalModules {
      modules = [
        module
        stubs
        { _module.args = { inherit pkgs; }; }
        { programs.shlv = cfg; }
      ];
    }).config;

  full = eval {
    enable = true;
    settings = {
      strict_tags = true;
      tags.canonical = {
        infra = {
          aliases = [ "ops" ];
          description = "servers, deployment, Nix, CI";
        };
        javascript = [
          "js"
          "ecmascript"
        ];
        rust = [ ];
      };
      tags.rules."docs.rs" = [ "rust" ];
    };
  };
  # `{ }` declares an empty list (it pins nothing); it must still render.
  emptyList = eval {
    enable = true;
    settings.tags.canonical = { };
  };
  noSettings = eval { enable = true; };
  # A provider whose key comes from a secret file (agenix-style); only the path is rendered.
  secrets = eval {
    enable = true;
    package = chosen;
    settings = {
      ai.tag = "claude";
      provider.claude = {
        kind = "anthropic";
        model = "claude-test";
        api_key_file = "/run/agenix/anthropic";
      };
    };
  };
  disabled = eval {
    enable = false;
    settings.strict_tags = true;
  };

  configOf = c: c.xdg.configFile."shlv/config.toml".source;

  # Evaluation-time assertions; a failure names the case.
  assertions = {
    "installs the package" = full.home.packages == [ shlv ] && lib.getName shlv == "shlv";
    "no config file without settings" = noSettings.xdg.configFile == { };
    "installs the package without settings" = noSettings.home.packages == [ shlv ];
    "does nothing when disabled" = disabled.home.packages == [ ] && disabled.xdg.configFile == { };
    "installs the chosen package" = secrets.home.packages == [ chosen ];
  };
  failed = lib.attrNames (lib.filterAttrs (_: ok: !ok) assertions);
in
assert lib.assertMsg (failed == [ ]) "hm-module: failed: ${lib.concatStringsSep ", " failed}";
pkgs.runCommand "shlv-hm-module" { nativeBuildInputs = [ shlv ]; } ''
  set -euo pipefail
  export HOME=$TMPDIR SHLV_DB=$TMPDIR/shlv.db

  check() { # name, expected, actual
    if [ "$2" != "$3" ]; then
      printf 'hm-module: %s\n--- expected\n%s\n--- actual\n%s\n' "$1" "$2" "$3" >&2
      exit 1
    fi
  }

  check "rendered TOML" '${''
    strict_tags = true

    [tags.canonical]
    javascript = ["js", "ecmascript"]
    rust = []

    [tags.canonical.infra]
    aliases = ["ops"]
    description = "servers, deployment, Nix, CI"

    [tags.rules]
    "docs.rs" = ["rust"]''}' "$(cat ${configOf full})"
  check "rendered empty list" '[tags.canonical]' "$(cat ${configOf emptyList})"

  # shlv reads the store file through a symlink, as home-manager installs it.
  mkdir -p $HOME/.config/shlv
  ln -s ${configOf full} $HOME/.config/shlv/config.toml
  check "config path" "$HOME/.config/shlv/config.toml" "$(shlv config path)"
  shlv config show >/dev/null

  # A tag in the database that the config declares as an alias gets merged into its tag.
  SHLV_CONFIG=/nonexistent shlv add https://example.com/a -t js >/dev/null
  shlv add https://example.com/b -t rust >/dev/null
  # The rule's quoted host survives rendering.
  check "rule suggestion" "3  https://docs.rs/serde  #rust (rule)" \
    "$(shlv add https://docs.rs/serde >/dev/null && shlv suggest ls)"
  check "synced list" "$(printf 'infra ops # servers, deployment, Nix, CI\njavascript ecmascript js\nrust')" "$(shlv tag canonical export)"
  check "alias merged" "1  https://example.com/a  #javascript" "$(shlv ls -t javascript)"
  if shlv add https://example.com/c -t nope 2>/dev/null; then
    echo "hm-module: strict_tags not applied" >&2
    exit 1
  fi

  # The declared list is a pinned base: tags, aliases, and descriptions can be added in shlv
  # without touching the read-only config, and strict mode accepts them.
  shlv tag canonical add nope >/dev/null
  shlv tag alias rs rust >/dev/null
  shlv tag describe nope "placeholders" >/dev/null
  shlv add https://example.com/c -t nope >/dev/null
  check "added in shlv" "$(printf 'infra ops # servers, deployment, Nix, CI\njavascript ecmascript js\nnope # placeholders\nrust rs')" "$(shlv tag canonical export)"
  check "export marks pinned tags" '  rust = [ "rs" ]; # pinned' "$(shlv tag canonical export --nix | grep rust)"
  # Undoing what the config declares is refused, naming the file.
  if err=$(shlv tag canonical rm rust 2>&1); then
    echo "hm-module: unmarking a pinned tag worked" >&2
    exit 1
  fi
  check "refusal names the config" "hint: change it in $HOME/.config/shlv/config.toml" "$(echo "$err" | tail -n 1)"

  # The provider section is parsed; `config show` prints the key file's path.
  ln -sf ${configOf secrets} $HOME/.config/shlv/config.toml
  check "key file path" 'api_key_file = "/run/agenix/anthropic"' "$(shlv config show | grep api_key_file)"

  # Switching to the empty list unpins everything but removes nothing; unpinned tags can be
  # unmarked.
  ln -sf ${configOf emptyList} $HOME/.config/shlv/config.toml
  check "empty list keeps tags" "4" "$(shlv tag canonical ls | wc -l)"
  shlv tag canonical rm rust >/dev/null
  check "unpinned tag unmarked" "infra javascript nope" "$(shlv tag canonical ls | tr '\n' ' ' | sed 's/ $//')"

  touch $out
''
