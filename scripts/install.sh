#!/usr/bin/env bash
# Builds and installs mic-filter for the current user, writes a starter
# config if there isn't one, and starts it as a systemd user service.
set -euo pipefail
cd "$(dirname "$0")/.."

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"

cargo install --locked --path crates/mic-filter

if [ ! -f "$config_home/mic-filter/config.toml" ]; then
    "$HOME/.cargo/bin/mic-filter" init
fi

install -Dm644 packaging/mic-filter.service "$config_home/systemd/user/mic-filter.service"
systemctl --user daemon-reload
systemctl --user enable --now mic-filter.service
systemctl --user restart mic-filter.service

echo
echo "mic-filter is running. Pick \"Mic Filter\" as the mic in your apps,"
echo "or make it the default:  pactl set-default-source mic_filter"
echo "Choose the real mic:     mic-filter use"
echo "Tune it live:            $config_home/mic-filter/config.toml"
echo "Watch levels:            systemctl --user stop mic-filter && mic-filter run --meter"
