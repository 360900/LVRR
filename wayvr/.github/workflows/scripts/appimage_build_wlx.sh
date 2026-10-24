#!/bin/sh
set -eu

cargo build --release
chmod +x ../target/release/wayvr
cp ../target/release/wayvr "${APPDIR}/usr/bin"
