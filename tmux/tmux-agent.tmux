#!/bin/sh
# tmux plugin entry point: tmux runs this with run-shell.
# The bin placeholder below is replaced with the binary's store path by nix/tmux-plugin.nix.
# Without Nix, the binary is found on PATH.
bin="@bin@"
case "$bin" in @*) bin=tmux-agent ;; esac
exec "$bin" init
