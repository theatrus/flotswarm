#!/usr/bin/env bash
# Build the distributor Lambda (Rust, provided.al2023, arm64) and zip it.
#
# We zip with `zip` (not archive_file / a plain zip of a copied file) so the
# 0755 exec bit on `bootstrap` is preserved — the custom runtime won't start it
# otherwise. Output: target/lambda/bootstrap/bootstrap.zip
set -euo pipefail
cd "$(dirname "$0")/.."

command -v cargo-lambda >/dev/null 2>&1 || {
    echo "cargo-lambda not found. Install: uv tool install cargo-lambda ziglang" >&2
    exit 1
}

cargo lambda build --release --arm64 -p flotswarm-distributor
cd target/lambda/bootstrap
rm -f bootstrap.zip
zip -j bootstrap.zip bootstrap
echo "built: $(pwd)/bootstrap.zip"
