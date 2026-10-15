#!/bin/sh
# Install the Linux build staged by scripts/build.sh into ~/.local:
# binary, hicolor icons, and a glide.desktop entry. Usage: scripts/install.sh
set -eu
cd "$(dirname "$0")/.."
SRC=dist/glide-linux
BIN_DIR="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor"
if [ ! -x "$SRC/glide" ]; then
  echo "error: $SRC/glide missing; run scripts/build.sh first" >&2
  exit 1
fi
mkdir -p "$BIN_DIR" "$APPS" "$ICONS"
install -m 755 "$SRC/glide" "$BIN_DIR/glide"
cp -r "$SRC/icons/hicolor/." "$ICONS/"
cat > "$APPS/glide.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Glide
Comment=Screen recorder and editor
Exec=$BIN_DIR/glide %F
Icon=glide
StartupWMClass=glide
Terminal=false
Categories=AudioVideo;Video;Recorder;
EOF
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$APPS" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -q -t "$ICONS" || true
fi
echo "installed $BIN_DIR/glide and $APPS/glide.desktop"
