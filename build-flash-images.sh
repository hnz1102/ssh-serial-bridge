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
source "$HOME/export-esp.sh"

mkdir -p "$OUTPUT_DIR"

for CONFIG in "${CONFIGS[@]}"; do
    echo "========================================"
    echo " Building: $CONFIG"
    echo "========================================"

    cp "$PROJECT_DIR/cfg.toml.$CONFIG" "$PROJECT_DIR/cfg.toml"

    (cd "$PROJECT_DIR" && cargo build --release)

    OUTPUT="$OUTPUT_DIR/flash_${CONFIG}.bin"
    (cd "$PROJECT_DIR" && cargo espflash save-image --chip esp32s3 --merge "$OUTPUT")

    # App-only image (no bootloader/partition-table) for OTA updates via
    # `cat app_<board>.bin | ssh admin@host update`. Do NOT use the merged
    # image above for OTA — it would corrupt the OTA partition.
    OTA_OUTPUT="$OUTPUT_DIR/app_${CONFIG}.bin"
    (cd "$PROJECT_DIR" && cargo espflash save-image --chip esp32s3 --release "$OTA_OUTPUT")

    echo "Flash image saved: $OUTPUT"
    echo "OTA app image saved: $OTA_OUTPUT"
    echo ""
done

echo "========================================"
echo " All builds complete"
echo " Output: $OUTPUT_DIR/"
ls -lh "$OUTPUT_DIR"/flash_*.bin "$OUTPUT_DIR"/app_*.bin
echo "========================================"
