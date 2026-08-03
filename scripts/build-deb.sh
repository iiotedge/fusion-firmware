#!/bin/bash
# scripts/build-deb.sh — packages a cross-compiled release binary as a
# .deb, for fleets that provision via apt/a local repo instead of the
# tarball make dist already produces (Phase 12, F10: Yocto/Debian
# packaging). Hand-rolled with dpkg-deb rather than cargo-deb: no extra
# cargo plugin to install in the build image, and dpkg-deb is already part
# of the Ubuntu base Dockerfile.cross builds on.
#
# Inputs (env vars, set by the Makefile's `deb` target):
#   VERSION       e.g. 1.1.0        (from Cargo.toml)
#   GIT_HASH      e.g. 3704ccd      (from git rev-parse --short HEAD)
#   DEB_ARCH      e.g. arm64        (Debian architecture name, NOT the Rust
#                                    target triple — see the Makefile)
#   RELEASE_BIN   path to the built fusion-firmware binary
#
# Installs to /opt/fusion-firmware, running as a dedicated `fusion-firmware`
# system user created by postinst — not root, and not auto-started (the
# shipped config still has CHANGE-ME secret placeholders; auto-starting a
# camera with default credentials is a real security foot-gun, not a
# hypothetical one — see README's "Before shipping a real device" note).
set -euo pipefail

: "${VERSION:?VERSION not set}"
: "${GIT_HASH:?GIT_HASH not set}"
: "${DEB_ARCH:?DEB_ARCH not set}"
: "${RELEASE_BIN:?RELEASE_BIN not set}"

PKG_NAME="fusion-firmware"
INSTALL_DIR="/opt/${PKG_NAME}"
SERVICE_USER="fusion-firmware"
STAGE="dist/deb/${PKG_NAME}_${VERSION}_${DEB_ARCH}"
OUT="dist/${PKG_NAME}_${VERSION}-${GIT_HASH}_${DEB_ARCH}.deb"

rm -rf "$STAGE"
mkdir -p \
  "$STAGE/DEBIAN" \
  "$STAGE${INSTALL_DIR}/bin" \
  "$STAGE${INSTALL_DIR}/config" \
  "$STAGE/lib/systemd/system"

cp "$RELEASE_BIN" "$STAGE${INSTALL_DIR}/bin/${PKG_NAME}"
cp config/iiotedge_default.toml "$STAGE${INSTALL_DIR}/config/"
sed \
  -e "s|@DEVICE_USER@|${SERVICE_USER}|g" \
  -e "s|@DEVICE_DIR@|${INSTALL_DIR}|g" \
  deploy/fusion-firmware.service > "$STAGE/lib/systemd/system/${PKG_NAME}.service"

# Runtime-lib package names (soname-suffixed, no -dev) — Dockerfile.cross's
# *-dev list is the compile-time counterpart. Exact suffixes (e.g. libssl3
# vs libssl3t64) can differ by target OS release; adjust for whatever the
# fleet's actual base image is before relying on this for a real rollout.
cat > "$STAGE/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}-${GIT_HASH}
Section: net
Priority: optional
Architecture: ${DEB_ARCH}
Maintainer: IIoTEdge <ops@iiotedge.example>
Depends: libgstreamer1.0-0, libgstreamer-plugins-base1.0-0, libgstreamer-plugins-good1.0-0, libgstreamer-plugins-bad1.0-0, libgstrtspserver-1.0-0, libglib2.0-0, libv4l-0, libudev1, libdbus-1-3, libssl3
Description: Fusion Firmware - edge vision, AI detection, cluster mesh, telemetry
 Config-driven edge vision platform: RTSP/ONVIF, on-device AI detection,
 cluster mesh, cloud relay, QR device onboarding, persist-first telemetry,
 and southbound machine-data ingestion.
EOF

cat > "$STAGE/DEBIAN/postinst" <<EOF
#!/bin/sh
set -e
if ! id ${SERVICE_USER} >/dev/null 2>&1; then
  adduser --system --group --no-create-home --home ${INSTALL_DIR} ${SERVICE_USER}
fi
chown -R ${SERVICE_USER}:${SERVICE_USER} ${INSTALL_DIR}
systemctl daemon-reload || true
echo "${PKG_NAME} installed to ${INSTALL_DIR}."
echo "Review ${INSTALL_DIR}/config/iiotedge_default.toml (CHANGE-ME placeholders!) before starting."
echo "Then: systemctl enable --now ${PKG_NAME}"
EOF

cat > "$STAGE/DEBIAN/prerm" <<EOF
#!/bin/sh
set -e
systemctl stop ${PKG_NAME} 2>/dev/null || true
systemctl disable ${PKG_NAME} 2>/dev/null || true
EOF

chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm"

mkdir -p dist
dpkg-deb --root-owner-group --build "$STAGE" "$OUT"
echo "✅ $OUT"
