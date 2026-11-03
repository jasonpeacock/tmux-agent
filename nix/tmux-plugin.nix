# The tmux plugin wrapper Home Manager's programs.tmux.plugins accepts:
# mkTmuxPlugin sets the tmuxplugin- pname and the .rtp attribute it requires.
{
  tmuxPlugins,
  tmux-agent,
  binPath ? "${tmux-agent}/bin/tmux-agent",
}:
tmuxPlugins.mkTmuxPlugin {
  pluginName = "tmux-agent";
  inherit (tmux-agent) version;
  src = ../tmux;
  rtpFilePath = "tmux-agent.tmux";
  postInstall = ''
    substituteInPlace $target/tmux-agent.tmux --subst-var-by bin ${binPath}
  '';
}
