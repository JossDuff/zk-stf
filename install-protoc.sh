#!/usr/bin/env bash
#
# Install a prebuilt protoc to /scratch (no root required).
# Needed for SP1 builds (sp1-prover-types uses prost-build).
#
# Usage:
#   ./install-protoc.sh              # installs to /scratch/protoc
#   INSTALL_DIR=~/opt ./install-protoc.sh   # override location

set -euo pipefail

PROTOC_VERSION="${PROTOC_VERSION:-25.9}"
INSTALL_DIR="${INSTALL_DIR:-/scratch/protoc}"

case "$(uname -m)" in
x86_64) ARCH="x86_64" ;;
aarch64) ARCH="aarch_64" ;;
*)
    echo "Unsupported arch: $(uname -m)" >&2
    exit 1
    ;;
esac

ZIP="protoc-${PROTOC_VERSION}-linux-${ARCH}.zip"
URL="https://github.com/protocolbuffers/protobuf/releases/download/v${PROTOC_VERSION}/${ZIP}"

if [[ -x "${INSTALL_DIR}/bin/protoc" ]]; then
    echo "protoc already installed at ${INSTALL_DIR}/bin/protoc"
    "${INSTALL_DIR}/bin/protoc" --version
    exit 0
fi

mkdir -p "$INSTALL_DIR"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "==> Downloading ${URL}"
curl -fL -o "${TMPDIR}/${ZIP}" "$URL"

echo "==> Extracting to ${INSTALL_DIR}"
unzip -q -o "${TMPDIR}/${ZIP}" -d "$INSTALL_DIR"

echo
"${INSTALL_DIR}/bin/protoc" --version
echo
echo "Add to your shell rc file:"
echo "  export PROTOC=${INSTALL_DIR}/bin/protoc"
