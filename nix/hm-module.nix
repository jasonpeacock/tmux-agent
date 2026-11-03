# Home Manager module: programs.tmux-agent.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.tmux-agent;
  system = pkgs.stdenv.hostPlatform.system;

  # tmux double-quoted string: escape the characters tmux treats specially.
  quote = s: ''"${lib.escape [ "\\" "\"" "$" ] s}"'';
  setOpt = name: value: "set -g @tmux-agent-${name} ${quote value}";
  onOff = b: if b then "on" else "off";

  # The binary, told which tmux to run so it never picks a different one
  # from PATH.
  wrapped = pkgs.symlinkJoin {
    name = "tmux-agent-wrapped-${cfg.package.version}";
    paths = [ cfg.package ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      wrapProgram $out/bin/tmux-agent \
        --set-default TMUX_AGENT_TMUX ${config.programs.tmux.package}/bin/tmux
    '';
    inherit (cfg.package) version meta;
  };

  # A path that survives generations and garbage collection. Hooks in a
  # running tmux server keep calling it after a switch.
  profileBin = "${config.home.profileDirectory}/bin/tmux-agent";

  tmuxPlugin = pkgs.callPackage ./tmux-plugin.nix {
    tmux-agent = wrapped;
    binPath = profileBin;
  };
  claudePlugin = pkgs.callPackage ./claude-plugin.nix {
    tmux-agent = wrapped;
    binPath = profileBin;
  };

  stateOpts = {
    needs-input = {
      icon = "󰂞";
      color = "#f38ba8";
    };
    done = {
      icon = "";
      color = "#a6e3a1";
    };
    working = {
      icon = "";
      color = "#f9e2af";
    };
  };
  mkStateOptions =
    state: defaults:
    lib.nameValuePair state {
      icon = lib.mkOption {
        type = lib.types.str;
        default = defaults.icon;
        description = "Tab icon for the ${state} state.";
      };
      color = lib.mkOption {
        type = lib.types.str;
        default = defaults.color;
        example = "#{@thm_red}";
        description = "Colour for the ${state} state: a tmux colour, or a format such as `#{@thm_red}`.";
      };
    };
in
{
  options.programs.tmux-agent = {
    enable = lib.mkEnableOption "Claude Code agent status on tmux window tabs";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.tmux-agent;
      defaultText = lib.literalExpression "tmux-agent.packages.\${system}.tmux-agent";
      description = "The tmux-agent package.";
    };

    tabMode = lib.mkOption {
      type = lib.types.enum [
        "options"
        "style"
      ];
      default = "options";
      description = ''
        `options`: only publish `@agent_tab_color` and `@agent_tab_icon` for the
        theme to read. `style`: also set a per-window `window-status-style`.
      '';
    };

    paneBorder = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Colour each agent pane's border by its state.";
    };

    states = lib.listToAttrs (lib.mapAttrsToList mkStateOptions stateOpts);

    keys = {
      jump = lib.mkOption {
        type = lib.types.str;
        default = "a";
        description = "Prefix key that jumps to the most urgent agent pane.";
      };
      pick = lib.mkOption {
        type = lib.types.str;
        default = "A";
        description = "Prefix key that opens the agent menu.";
      };
    };

    chooseTree = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Rebind `prefix w` to a choose-tree that shows agent icons.";
    };

    claudePlugin.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Install the Claude Code plugin through
        `programs.claude-code.plugins`. Requires `programs.claude-code.enable`;
        `programs.claude-code.package = null` works with a Claude installed
        outside Nix.
      '';
    };

    statusRightSnippet = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
      default = "#{@agent_summary}#(#{@agent_bin} reconcile)";
      description = ''
        Append to `status-right` in your own config, after your `status-right`
        is set: `set -ag status-right "''${config.programs.tmux-agent.statusRightSnippet}"`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ wrapped ];

    programs.tmux.plugins = [
      {
        plugin = tmuxPlugin;
        extraConfig = lib.concatStringsSep "\n" (
          [
            (setOpt "bin" profileBin)
            (setOpt "tab-mode" cfg.tabMode)
            (setOpt "pane-border" (onOff cfg.paneBorder))
            (setOpt "jump-key" cfg.keys.jump)
            (setOpt "pick-key" cfg.keys.pick)
            (setOpt "choose-tree" (onOff cfg.chooseTree))
          ]
          ++ lib.concatLists (
            lib.mapAttrsToList (state: v: [
              (setOpt "icon-${state}" v.icon)
              (setOpt "color-${state}" v.color)
            ]) cfg.states
          )
        );
      }
    ];

    programs.claude-code.plugins = lib.mkIf cfg.claudePlugin.enable {
      tmux-agent = claudePlugin;
    };
  };
}
