#!/bin/bash
set -e

BINARY_NAME="git_mcp"
TARGET_DIR="target/release"
INSTALL_DIR="/usr/local/bin"

echo "🦀 Building $BINARY_NAME (Release Mode)..."
cargo build --release

echo "📦 Installing to $INSTALL_DIR..."
if [ -w "$INSTALL_DIR" ]; then
    cp "$TARGET_DIR/$BINARY_NAME" "$INSTALL_DIR/"
else
    sudo cp "$TARGET_DIR/$BINARY_NAME" "$INSTALL_DIR/"
fi

echo "✅ Success! Installed"