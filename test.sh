#!/usr/bin/env bash

set -euo pipefail

while true; do
    if ! cargo run -- -r carlossless-chedar attach --vendor_id 0x0603 --product_id 0x1020; then
        echo "=== 0x0603:0x1020 not found"
    fi

    if ! cargo run -- -r carlossless-chedar attach --vendor_id 0x05ac --product_id 0x024f; then
        echo "=== 0x05ac:0x024f not found"
    fi
done