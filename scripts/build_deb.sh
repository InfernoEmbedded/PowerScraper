#!/usr/bin/env bash
set -e

ARCH=$1
TARGET=$2

if [ -z "$ARCH" ] || [ -z "$TARGET" ]; then
    echo "Usage: $0 <debian-arch> <rust-target>"
    echo "Example: $0 amd64 x86_64-unknown-linux-gnu"
    exit 1
fi

echo "Building PowerScraper release binary for $TARGET..."
if [ "$TARGET" = "aarch64-unknown-linux-gnu" ]; then
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc cargo build --release --target "$TARGET"
elif [ "$TARGET" = "x86_64-unknown-linux-gnu" ]; then
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc cargo build --release --target "$TARGET"
else
    cargo build --release --target "$TARGET"
fi

# Define paths
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION=$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -n1 | cut -d '"' -f2)
PKG_NAME="powerscraper"
BUILD_DIR="$PROJECT_ROOT/target/debian_build_${ARCH}_$$"
OUT_DIR="$PROJECT_ROOT/dist"

echo "Creating Debian package structure in $BUILD_DIR..."
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR/usr/bin"
mkdir -p "$BUILD_DIR/lib/systemd/system"
mkdir -p "$BUILD_DIR/var/lib/powerscraper"
mkdir -p "$BUILD_DIR/DEBIAN"

chmod 755 "$BUILD_DIR"
chmod 755 "$BUILD_DIR/usr" "$BUILD_DIR/usr/bin"
chmod 755 "$BUILD_DIR/lib" "$BUILD_DIR/lib/systemd" "$BUILD_DIR/lib/systemd/system"
chmod 755 "$BUILD_DIR/var" "$BUILD_DIR/var/lib" "$BUILD_DIR/var/lib/powerscraper"
chmod 755 "$BUILD_DIR/DEBIAN"

# Copy compiled binary
cp "$PROJECT_ROOT/target/$TARGET/release/PowerScraper" "$BUILD_DIR/usr/bin/powerscraper"

# Copy Python scripts for evolutionary tuning
mkdir -p "$BUILD_DIR/usr/share/powerscraper/scripts"
cp "$PROJECT_ROOT/scripts/evolutionary_optimizer.py" "$BUILD_DIR/usr/share/powerscraper/scripts/evolutionary_optimizer.py"
cp "$PROJECT_ROOT/scripts/battery_simulation.py" "$BUILD_DIR/usr/share/powerscraper/scripts/battery_simulation.py"
chmod +x "$BUILD_DIR/usr/share/powerscraper/scripts/evolutionary_optimizer.py"
chmod +x "$BUILD_DIR/usr/share/powerscraper/scripts/battery_simulation.py"

# Create systemd service file
cat << 'EOF' > "$BUILD_DIR/lib/systemd/system/powerscraper.service"
[Unit]
Description=PowerScraper daemon
After=network.target

[Service]
Type=exec
User=root
WorkingDirectory=/var/lib/powerscraper
ExecStart=/usr/bin/powerscraper
Restart=always
RestartSec=3
TimeoutStopSec=10s
KillMode=mixed
LimitRTPRIO=99

[Install]
WantedBy=multi-user.target
EOF

# Create DEBIAN/control
cat << EOF > "$BUILD_DIR/DEBIAN/control"
Package: $PKG_NAME
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Depends: libc6
Maintainer: Inferno Embedded <http://infernoembedded.com>
Description: PowerScraper Daemon
 Multithreaded service to scrape solar inverter telemetry, regulate grid import/export, and forward data.
EOF

# Create DEBIAN/postinst
cat << 'EOF' > "$BUILD_DIR/DEBIAN/postinst"
#!/bin/sh
set -e

if [ "$1" = "configure" ]; then
    # Ensure database dir exists
    mkdir -p /var/lib/powerscraper
    chmod 750 /var/lib/powerscraper

    # Reload systemd and start service
    systemctl daemon-reload
    systemctl enable powerscraper.service || true
    systemctl restart powerscraper.service || true
fi
EOF
chmod 755 "$BUILD_DIR/DEBIAN/postinst"

# Create DEBIAN/prerm
cat << 'EOF' > "$BUILD_DIR/DEBIAN/prerm"
#!/bin/sh
set -e

if [ "$1" = "remove" ] || [ "$1" = "deconfigure" ]; then
    systemctl stop powerscraper.service || true
    systemctl disable powerscraper.service || true
fi
EOF
chmod 755 "$BUILD_DIR/DEBIAN/prerm"

# Create DEBIAN/postrm
cat << 'EOF' > "$BUILD_DIR/DEBIAN/postrm"
#!/bin/sh
set -e

if [ "$1" = "purge" ] || [ "$1" = "remove" ]; then
    systemctl daemon-reload
fi
EOF
chmod 755 "$BUILD_DIR/DEBIAN/postrm"

# Build package
mkdir -p "$OUT_DIR"
chmod 775 "$OUT_DIR" || true
DEB_FILE="$OUT_DIR/${PKG_NAME}_${VERSION}_${ARCH}.deb"
TMP_DEB="/tmp/${PKG_NAME}_${VERSION}_${ARCH}_$$.deb"
rm -f "$DEB_FILE" || true

echo "Building package using dpkg-deb..."
dpkg-deb --build "$BUILD_DIR" "$TMP_DEB"
if ! cp -f "$TMP_DEB" "$DEB_FILE"; then
    echo "ERROR: Failed to copy $TMP_DEB to $DEB_FILE"
    exit 1
fi
rm -f "$TMP_DEB"

# Clean up
rm -rf "$BUILD_DIR"

echo "Debian package successfully built: $DEB_FILE"
