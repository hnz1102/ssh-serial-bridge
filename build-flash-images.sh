#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$SCRIPT_DIR"
OUTPUT_DIR="$SCRIPT_DIR/flash_images"

CONFIGS=(
    "xiao-esp32s3"
    "ssh-bridge-board"
    "mini-ssh-bridge-board"
)

# Source ESP toolchain environment
source "$SCRIPT_DIR/../export-esp.sh"

mkdir -p "$OUTPUT_DIR"

for CONFIG in "${CONFIGS[@]}"; do
    echo "========================================"
    echo " Building: $CONFIG"
    echo "========================================"

    cp "$PROJECT_DIR/cfg.toml.$CONFIG" "$PROJECT_DIR/cfg.toml"

    (cd "$PROJECT_DIR" && cargo build --release)

    OUTPUT="$OUTPUT_DIR/flash_${CONFIG}.bin"
    (cd "$PROJECT_DIR" && cargo espflash save-image --chip esp32s3 --merge "$OUTPUT")

    echo "Flash image saved: $OUTPUT"
    echo ""
done

echo "========================================"
echo " All builds complete"
echo " Output: $OUTPUT_DIR/"
ls -lh "$OUTPUT_DIR"/flash_*.bin
echo "========================================"
