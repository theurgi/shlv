# Changelog

User-facing changes to shlv. Versions follow [SemVer](https://semver.org/) while below 1.0: a minor
version can break scripts (CLI, config, or export format), and those changes are marked
**Breaking**.

## Unreleased

- `api_key_cmd` in a `[provider.*]` section reads the API key from a command, such as a secret
  manager (`["secret-tool", "lookup", "service", "shlv"]`). It runs once per process, when the first
  request needs the key; in the TUI, with the screen suspended so prompts work.

## 0.1.0

Everything up to the first versioning policy (task 050): storage, the CLI, tags, import and export,
fetching, AI tagging, the Nix package, and the TUI.
