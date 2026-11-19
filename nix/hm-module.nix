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
          or an alias of a canonical tag. With `"propose"`, they become proposals for the list
          instead, reviewed with `shlv tag proposals`.
        - `tags.canonical`: the declared canonical tag list, mapping each tag to its aliases, or
          to `{ aliases = [ ... ]; description = "..."; }`. A description (one line, at most 200
          characters) says what belongs under the tag; the tag suggestion layers match against it.
          The database holds the canonical list; this is a pinned base under it. On startup,
          declared tags become canonical and their aliases point at them. Tags, aliases, and
          descriptions added with `shlv tag ...` stay, so adding a tag needs no rebuild, and a tag
          removed from this list stays canonical (no longer pinned). shlv refuses to rename, merge
          away, or unmark a declared tag, remove a declared alias, or change a declared
          description. `shlv tag canonical export --nix` prints the whole list in this shape to
          copy back here. Names must be normalized tags (`"web-dev"`, not `"Web Dev"`).
        - `tags.rules`: domain rules mapping a host (which also matches its subdomains) to tags,
          as in `tags.rules."github.com" = [ "code" ]`. `shlv add` and `shlv suggest` turn them
          into tag suggestions. With `strict_tags`, rule tags must be canonical.
        - `tags.suggestions`: `"confirm"` (the default) keeps suggestions for review with
          `shlv suggest ls|accept|reject`; `"auto"` applies them at once.
        - `tags.style`: how tags are written. `separator` (`"-"`, `"_"`, or `""`) is what spaces
          and the other separators in new tag names become; `max_length` refuses longer new
          names. Declared names and rule tags must already follow them. `guidelines` is plain
          text sent to the AI with every tagging request.
        - `ai` and `provider`: `ai.summarize` and `ai.tag` name a `provider.<name>` with `kind`
          (`"openai-compatible"` or `"anthropic"`), `model`, `base_url`, one of `api_key_env`,
          `api_key_file`, or `api_key_cmd`, `timeout`, and for `"anthropic"` an optional
          `effort`. AI is off until `ai` names a provider. For each bookmark it processes, shlv sends the
          provider its URL, title, note, summary, and extracted page text, and for tagging the
          canonical tag list. Nothing is sent unless a provider is configured.

        Secrets must not go here: everything in `settings` ends up in the world-readable Nix
        store. Providers name where their key is instead: an environment variable
        (`provider.claude.api_key_env = "ANTHROPIC_API_KEY"`) or a file that shlv reads only
        when the provider is used, such as an agenix or sops-nix secret:
        `provider.claude.api_key_file = config.age.secrets.anthropic.path`. Only the path
        reaches the store. shlv trims the file's trailing newline and warns if every user can
        read it. Or a command that prints the key, run without a shell when the first request
        needs it: `provider.claude.api_key_cmd = [ "secret-tool" "lookup" "service" "shlv" ]`.
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
