# The Claude Code plugin folder, with the binary path baked in as the
# `binary` setting's default.
{
  lib,
  runCommand,
  jq,
  tmux-agent,
  binPath ? "${tmux-agent}/bin/tmux-agent",
}:
runCommand "tmux-agent-claude-plugin-${tmux-agent.version}"
  {
    src = lib.fileset.toSource {
      root = ../plugin;
      fileset = ../plugin;
    };
    nativeBuildInputs = [ jq ];
  }
  ''
    cp -r $src $out
    chmod -R u+w $out
    jq --arg bin ${lib.escapeShellArg binPath} '.userConfig.binary.default = $bin' \
      $src/.claude-plugin/plugin.json > $out/.claude-plugin/plugin.json
  ''
