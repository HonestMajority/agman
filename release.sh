#!/usr/bin/env bash
set -euo pipefail

# Default output directory
DEFAULT_DIR="$HOME/commands"
OUTPUT_DIR="${1:-$DEFAULT_DIR}"

# Check if output directory exists
if [[ ! -d "$OUTPUT_DIR" ]]; then
    if [[ -z "${1:-}" ]]; then
        echo "Error: $DEFAULT_DIR does not exist."
        echo "Usage: $0 [output-directory]"
        exit 1
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

echo "Done! agman installed at $OUTPUT_DIR/agman"
