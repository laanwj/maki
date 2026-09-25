#!/bin/sh
# Run the brain interactively (it is the TUI). The executor service owns the
# socket; start it first with `systemctl --user start maki-executor`.
#
# Keys come from brain.env on the host (0600, one KEY=value per line). They
# are visible to same-user host tooling (`podman inspect`, the env file), the
# same exposure class as maki's own 0600 auth files.
set -eu

exec podman run --rm -it \
  --name maki-brain \
  -v "$XDG_RUNTIME_DIR/maki-split-example:/run/maki-split" \
  -v "$HOME/.config/maki:/home/maki/.config/maki" \
  -v "$HOME/.local/state/maki:/home/maki/.local/state/maki" \
  --env-file "$HOME/.config/maki/brain.env" \
  localhost/maki-brain:latest "$@"
