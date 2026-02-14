#!/usr/bin/env bash
set -euo pipefail

# Default output directory
DEFAULT_DIR="$HOME/.agman/bin"
OUTPUT_DIR="${1:-$DEFAULT_DIR}"

# Ensure output directory exists (auto-create for default, error for custom)
if [[ ! -d "$OUTPUT_DIR" ]]; then
    if [[ -z "${1:-}" ]]; then
        mkdir -p "$OUTPUT_DIR"
    else
        echo "Error: $OUTPUT_DIR does not exist."
        exit 1
    fi
fi

# Build in release mode
echo "Building agman in release mode..."
cargo build --release

# Copy binary to output directory
echo "Installing to $OUTPUT_DIR/agman..."
cp target/release/agman "$OUTPUT_DIR/agman"

# Remove quarantine attributes and ad-hoc sign (macOS)
if [[ "$(uname)" == "Darwin" ]]; then
    echo "Signing binary for macOS..."
    xattr -cr "$OUTPUT_DIR/agman" 2>/dev/null || true
    codesign -s - "$OUTPUT_DIR/agman" 2>/dev/null || true
fi

# Reinitialize agman config files
echo "Running agman init --force..."
agman init --force

# Check if the default install dir is in $PATH (only for default dir)
if [[ -z "${1:-}" ]] && [[ ":$PATH:" != *":$OUTPUT_DIR:"* ]]; then
    echo ""
    echo "NOTE: $OUTPUT_DIR is not in your \$PATH. Add this to your shell profile:"
    echo "  export PATH=\"$OUTPUT_DIR:\$PATH\""
    echo ""
fi

echo "Done! agman installed at $OUTPUT_DIR/agman"
