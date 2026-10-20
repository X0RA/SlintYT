#!/usr/bin/env bash
set -euo pipefail

# Set SLINTYT_REPO once a GitHub repository exists, or replace this placeholder.
REPO="${SLINTYT_REPO:-OWNER/REPOSITORY}"
VERSION="${SLINTYT_VERSION:-latest}"
PREFIX="${PREFIX:-/usr/local}"
if [[ "$REPO" == "OWNER/REPOSITORY" ]]; then
    echo 'No upstream configured. Use make install from a local build, or set SLINTYT_REPO=owner/repo.' >&2
    exit 1
fi
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
    echo 'This installer requires Linux x86_64.' >&2
    exit 1
fi
for tool in curl sha256sum install; do
    command -v "$tool" >/dev/null || { echo "$tool is required." >&2; exit 1; }
done
if [[ "$PREFIX" != /* ]]; then
    echo 'PREFIX must be an absolute path.' >&2
    exit 1
fi
if [[ "$VERSION" == latest ]]; then
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
    BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
fi
WORK_DIR="$(mktemp -d -t slintyt-install.XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT
assets=(slintyt-linux-x86_64 slintyt.desktop slintyt.png slintyt.svg)
for asset in SHA256SUMS "${assets[@]}"; do
    curl --fail --location --proto '=https' --tlsv1.2 --retry 3 \
        --output "$WORK_DIR/$asset" "$BASE_URL/$asset"
done
for asset in "${assets[@]}"; do
    awk -v file="$asset" '$2 == file { print }' "$WORK_DIR/SHA256SUMS" > "$WORK_DIR/checksum"
    test -s "$WORK_DIR/checksum" || { echo "Missing checksum for $asset" >&2; exit 1; }
    (cd "$WORK_DIR" && sha256sum --check checksum)
done

SUDO=()
if [[ ! -w "$PREFIX" && ! ( ! -e "$PREFIX" && -w "$(dirname "$PREFIX")" ) ]]; then
    command -v sudo >/dev/null || { echo "sudo is needed to write $PREFIX" >&2; exit 1; }
    SUDO=(sudo)
fi
"${SUDO[@]}" install -Dm755 "$WORK_DIR/slintyt-linux-x86_64" "$PREFIX/bin/slintyt"
"${SUDO[@]}" install -Dm644 "$WORK_DIR/slintyt.desktop" "$PREFIX/share/applications/slintyt.desktop"
"${SUDO[@]}" install -Dm644 "$WORK_DIR/slintyt.png" "$PREFIX/share/icons/hicolor/256x256/apps/slintyt.png"
"${SUDO[@]}" install -Dm644 "$WORK_DIR/slintyt.svg" "$PREFIX/share/icons/hicolor/scalable/apps/slintyt.svg"
if command -v update-desktop-database >/dev/null; then
    "${SUDO[@]}" update-desktop-database "$PREFIX/share/applications"
fi
if command -v gtk-update-icon-cache >/dev/null; then
    "${SUDO[@]}" gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor"
fi
if ! command -v yt-dlp >/dev/null; then
    echo 'Install yt-dlp and a supported JavaScript runtime (such as Deno) for YouTube playback.' >&2
fi
if ! { command -v ldconfig >/dev/null && ldconfig -p 2>/dev/null | grep 'libmpv\.so' >/dev/null; }; then
    echo 'Install libmpv if needed (libmpv2 on Ubuntu, mpv on Arch, mpv-libs on Fedora).' >&2
fi
printf 'Installed SlintYT under %s. Ensure %s/bin is on your desktop session PATH.\n' "$PREFIX" "$PREFIX"
