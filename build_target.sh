#!/bin/bash
set -e

echo "Starting IIoTEdge Cross-Compilation for i.MX 8M Plus..."

# 1. Build the Docker image (only takes time on the first run)
docker build -t iiotedge-builder -f Dockerfile.cross .

# 2. Run the compiler inside the container
# We mount your current directory ($PWD) into the container's /workspace
# This means the compiled binary will appear right on your local machine.
docker run --rm -v "$PWD":/workspace iiotedge-builder bash -c "
    echo 'Compiling for aarch64-unknown-linux-gnu...' &&
    cargo build --release --target aarch64-unknown-linux-gnu
"

echo "✅ Build Complete!"
echo "Your production binary is located at: target/aarch64-unknown-linux-gnu/release/iiotedge-firmware"