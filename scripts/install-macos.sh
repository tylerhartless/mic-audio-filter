#!/usr/bin/env bash
# Builds and installs mic-filter on macOS: "Mic Filter.app" in ~/Applications,
# a starter config if there isn't one, and a launchd agent that runs it at
# login. install.sh runs this on a Mac.
set -euo pipefail
cd "$(dirname "$0")/.."

label=io.github.tylerhartless.mic-filter
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
app="$HOME/Applications/Mic Filter.app"
bin="$app/Contents/MacOS/mic-filter"
agent="$HOME/Library/LaunchAgents/$label.plist"
log="$HOME/Library/Logs/mic-filter.log"

if ! ls -d /Library/Audio/Plug-Ins/HAL/BlackHole*.driver > /dev/null 2>&1; then
    echo "BlackHole isn't installed. mic-filter plays your processed voice into it,"
    echo "and apps record from it. Install it, then run this script again:"
    echo
    echo "  brew install blackhole-2ch"
    exit 1
fi

cargo install --locked --path crates/mic-filter

# Stop a running copy first: macOS kills a process whose signed binary
# changes under it.
launchctl bootout "gui/$(id -u)/$label" 2> /dev/null || true

# The app bundle gives the window a Dock icon and gives macOS a name to show
# when it asks for microphone access. The agent runs the same binary, so one
# permission covers both.
mkdir -p "$app/Contents/MacOS"
rm -f "$bin"
cp "$HOME/.cargo/bin/mic-filter" "$bin"
version="$("$bin" --version | awk '{ print $2 }')"
sed "s|@VERSION@|$version|g" crates/mic-filter/macos/Info.plist > "$app/Contents/Info.plist"
codesign --force --sign - "$app"

if [ ! -f "$config_home/mic-filter/config.toml" ]; then
    "$bin" init
fi

mkdir -p "$(dirname "$agent")" "$(dirname "$log")"
sed -e "s|@BIN@|$bin|" -e "s|@LOG@|$log|" packaging/macos/mic-filter.plist > "$agent"
launchctl bootstrap "gui/$(id -u)" "$agent"

echo
echo "mic-filter is running. If macOS asks whether Mic Filter may use the"
echo "microphone, allow it."
echo "Pick \"BlackHole 2ch\" as the mic in your apps."
echo "Tune it: open Mic Filter from ~/Applications, or run: mic-filter gui"
echo "Log:     $log"
