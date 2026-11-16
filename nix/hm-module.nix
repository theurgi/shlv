# Home-manager module: `programs.shlv`. The flake applies the first argument, so users import
# `shlv.homeManagerModules.default`.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.shlv;
  tomlFormat = pkgs.formats.toml { };
in
{
  options.programs.shlv = {
    enable = lib.mkEnableOption "shlv, a command-line bookmark manager";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.shlv;
      defaultText = lib.literalExpression "shlv.packages.\${pkgs.stdenv.hostPlatform.system}.shlv";
      description = "The shlv package to install.";
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = { };
      example = lib.literalExpression ''
        {
          strict_tags = true;
          tags.canonical = {
            javascript = [ "js" "ecmascript" ];
            rust = [ ];
            infra = {
              aliases = [ "ops" ];
              description = "servers, deployment, Nix, CI";
            };
          };
        }
      '';
      description = ''
        Configuration written to {file}`$XDG_CONFIG_HOME/shlv/config.toml`. Nothing is written
        when this is empty. shlv only reads the file, so a read-only symlink into the Nix store
        works. Unknown keys are errors when shlv loads the file. Run `shlv config show` to see
        what it read.

        - `strict_tags`: when true, `add`, `edit`, and `import` reject tags that aren't canonical
          or an alias of a canonical tag.
        - `tags.canonical`: the declared canonical tag list, mapping each tag to its aliases, or
          to `{ aliases = [ ... ]; description = "..."; }`. A description (one line, at most 200
          characters) says what belongs under the tag; the tag suggestion layers match against it.
          When set, even to `{ }`, this list is the source of truth: on startup the database
          mirrors it (declared tags become canonical, aliases point at them, other canonical tags
          are unmarked; bookmarks keep their tags), and `shlv tag canonical add|rm|import` and
          `shlv tag alias` are refused. Names must be normalized tags (`"web-dev"`, not
          `"Web Dev"`).
        - `tags.rules`: domain rules mapping a host (which also matches its subdomains) to tags,
          as in `tags.rules."github.com" = [ "code" ]`. `shlv add` and `shlv suggest` turn them
          into tag suggestions. With `strict_tags`, rule tags must be canonical.
        - `tags.suggestions`: `"confirm"` (the default) keeps suggestions for review with
          `shlv suggest ls|accept|reject`; `"auto"` applies them at once.
        - `ai` and `provider` (builds with the `ai` feature, such as `shlv-full`): `ai.summarize`
          and `ai.tag` name a `provider.<name>` with `kind` (`"openai-compatible"` or
          `"anthropic"`), `model`, `base_url`, `api_key_env` or `api_key_file`, `timeout`, and for
          `"anthropic"` an optional `effort`. AI is
          off until `ai` names a provider. For each bookmark it processes, shlv sends the
          provider its URL, title, note, summary, and extracted page text, and for tagging the
          canonical tag list. Nothing is sent unless a provider is configured.

        Secrets must not go here: everything in `settings` ends up in the world-readable Nix
        store. Providers name where their key is instead: an environment variable
        (`provider.claude.api_key_env = "ANTHROPIC_API_KEY"`) or a file that shlv reads only
        when the provider is used, such as an agenix or sops-nix secret:
        `provider.claude.api_key_file = config.age.secrets.anthropic.path`. Only the path
        reaches the store. shlv trims the file's trailing newline and warns if every user can
        read it.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."shlv/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = tomlFormat.generate "shlv-config.toml" cfg.settings;
    };
  };
}
