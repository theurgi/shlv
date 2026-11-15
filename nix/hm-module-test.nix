# Flake check for the home-manager module. Evaluates it with `lib.evalModules` and stubs for the two
# home-manager options it sets, then has shlv load the rendered config and mirror the tag list.
{
  lib,
  pkgs,
  module,
}:
let
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
        javascript = [
          "js"
          "ecmascript"
        ];
        rust = [ ];
      };
    };
  };
  # `{ }` still declares the list (and unmarks every canonical tag), so it must render.
  emptyList = eval {
    enable = true;
    settings.tags.canonical = { };
  };
  noSettings = eval { enable = true; };
  disabled = eval {
    enable = false;
    settings.strict_tags = true;
  };

  configOf = c: c.xdg.configFile."shlv/config.toml".source;
  shlv = lib.head full.home.packages;

  # Evaluation-time assertions; a failure names the case.
  assertions = {
    "installs the package" = full.home.packages == [ shlv ] && lib.getName shlv == "shlv";
    "no config file without settings" = noSettings.xdg.configFile == { };
    "installs the package without settings" = noSettings.home.packages == [ shlv ];
    "does nothing when disabled" = disabled.home.packages == [ ] && disabled.xdg.configFile == { };
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
    rust = []''}' "$(cat ${configOf full})"
  check "rendered empty list" '[tags.canonical]' "$(cat ${configOf emptyList})"

  # shlv reads the store file through a symlink, as home-manager installs it.
  mkdir -p $HOME/.config/shlv
  ln -s ${configOf full} $HOME/.config/shlv/config.toml
  check "config path" "$HOME/.config/shlv/config.toml" "$(shlv config path)"
  shlv config show >/dev/null

  # A tag in the database that the config declares as an alias gets merged into its tag.
  SHLV_CONFIG=/nonexistent shlv add https://example.com/a -t js >/dev/null
  shlv add https://example.com/b -t rust >/dev/null
  check "mirrored list" "$(printf 'javascript ecmascript js\nrust')" "$(shlv tag canonical export)"
  check "alias merged" "1  https://example.com/a  #javascript" "$(shlv ls -t javascript)"
  if shlv add https://example.com/c -t nope 2>/dev/null; then
    echo "hm-module: strict_tags not applied" >&2
    exit 1
  fi

  # The empty list unmarks everything.
  ln -sf ${configOf emptyList} $HOME/.config/shlv/config.toml
  check "empty list" "" "$(shlv tag canonical export)"

  touch $out
''
