#!/usr/bin/env bash
# Builds and installs mic-filter for the current user, writes a starter
# config if there isn't one, adds "Mic Filter" to the app menu, and starts it
# as a systemd user service. On macOS it runs install-macos.sh instead.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "$(uname -s)" = Darwin ]; then
    exec scripts/install-macos.sh
fi

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
apps_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
bin="$HOME/.cargo/bin/mic-filter"

cargo install --locked --path crates/mic-filter

if [ ! -f "$config_home/mic-filter/config.toml" ]; then
    "$bin" init
fi

mkdir -p "$apps_dir"
sed "s|@BIN@|$bin|" packaging/mic-filter.desktop > "$apps_dir/mic-filter.desktop"
update-desktop-database "$apps_dir" 2>/dev/null || true

install -Dm644 packaging/mic-filter.service "$config_home/systemd/user/mic-filter.service"
systemctl --user daemon-reload
systemctl --user enable --now mic-filter.service
systemctl --user restart mic-filter.service

echo
echo "mic-filter is running. Pick \"Mic Filter\" as the mic in your apps,"
echo "or make it the default:  pactl set-default-source mic_filter"
echo "Tune it:                 open \"Mic Filter\" from the app menu, or run: mic-filter gui"
