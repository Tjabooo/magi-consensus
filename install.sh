#!/bin/sh
# Install `consensus` for the current user: ~/.local/share/magi-consensus/bin + a link in ~/.local/bin.
# CONSENSUS_BINARY=path/to/consensus ./install.sh installs a prebuilt binary instead of building.
set -eu
cd "$(dirname "$0")"
share="${XDG_DATA_HOME:-$HOME/.local/share}/magi-consensus/bin"
bin="$HOME/.local/bin"
if [ -z "${CONSENSUS_BINARY:-}" ]; then
    nice -n 19 cargo build --release --locked
    CONSENSUS_BINARY=target/release/consensus
fi
mkdir -p "$share" "$bin"
install -m755 "$CONSENSUS_BINARY" "$share/consensus.new"
mv -f "$share/consensus.new" "$share/consensus"
ln -sf "$share/consensus" "$bin/consensus"
cfg="${XDG_CONFIG_HOME:-$HOME/.config}/magi-consensus/config.toml"
if [ ! -e "$cfg" ]; then
    mkdir -p "$(dirname "$cfg")"
    cp examples/config.toml "$cfg"
    echo "wrote $cfg (edit the endpoint and tools)"
fi
echo "installed: $("$bin/consensus" --version)"
