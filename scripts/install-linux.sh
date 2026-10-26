#!/usr/bin/env bash
set -euo pipefail

# Override SLINTYT_REPO to install from a fork.
REPO="${SLINTYT_REPO:-X0RA/SlintYT}"
VERSION="${SLINTYT_VERSION:-latest}"
PREFIX="${PREFIX:-/usr/local}"
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
    curl --fail --location --proto '=https' --tlsv1.2 --connect-timeout 15 --max-time 180 --retry 3 --retry-max-time 300 \
        --output "$WORK_DIR/$asset" "$BASE_URL/$asset"
done
for asset in "${assets[@]}"; do
    awk -v file="$asset" '$2 == file { print }' "$WORK_DIR/SHA256SUMS" > "$WORK_DIR/checksum"
    test -s "$WORK_DIR/checksum" || { echo "Missing checksum for $asset" >&2; exit 1; }
    (cd "$WORK_DIR" && sha256sum --check checksum)
done

# All privileged work happens in this one function, so an update asks for a
# password at most once.
install_files() (
    set -euo pipefail
    local work="$1" prefix="$2" stage destination index committed=0 rollback_failed=0
    local lock="$prefix/.slintyt-install-lock"
    local -a destinations=(
        "$prefix/bin/slintyt"
        "$prefix/share/applications/slintyt.desktop"
        "$prefix/share/icons/hicolor/256x256/apps/slintyt.png"
        "$prefix/share/icons/hicolor/scalable/apps/slintyt.svg"
    )
    local -a sources=(slintyt-linux-x86_64 slintyt.desktop slintyt.png slintyt.svg)
    local -a stages=()
    mkdir -p "$prefix"
    mkdir "$lock" || { echo "Another install is active. If none is, remove $lock and try again." >&2; exit 1; }
    cleanup_install() {
        local status=$?
        trap - EXIT HUP INT TERM
        if (( ! committed )); then
            for (( index=${#stages[@]}-1; index>=0; index-- )); do
                stage="${stages[index]}"
                destination="${destinations[index]}"
                # A missing staged file means its rename has taken place.
                if [[ -e "$stage/ready" && ! -e "$stage/new" ]]; then
                    if [[ -e "$stage/old" || -L "$stage/old" ]]; then
                        mv -Tf "$stage/old" "$destination" || rollback_failed=1
                    else
                        rm -f "$destination" || rollback_failed=1
                    fi
                fi
            done
        fi
        if (( rollback_failed )); then
            echo "Rollback failed. Restore the files from the backups in ${stages[*]}, then remove $lock." >&2
            exit 1
        fi
        # A leftover backup is harmless, but a leftover lock blocks every later install.
        for stage in "${stages[@]}"; do rm -rf "$stage" || echo "Warning: could not remove $stage" >&2; done
        rmdir "$lock" || echo "Warning: could not remove $lock. Remove it before installing again." >&2
        exit "$status"
    }
    trap cleanup_install EXIT
    trap 'exit 1' HUP INT TERM
    # Stage beside each destination so every replacement is a same-filesystem rename.
    for index in "${!destinations[@]}"; do
        destination="${destinations[index]}"
        [[ ! -d "$destination" ]] || { echo "Not a file: $destination" >&2; exit 1; }
        mkdir -p "$(dirname "$destination")"
        stage="$(mktemp -d "$(dirname "$destination")/.slintyt-update.XXXXXX")"
        stages+=("$stage")
        if [[ -e "$destination" || -L "$destination" ]]; then
            cp -a "$destination" "$stage/old"
        fi
        local mode=644
        (( index != 0 )) || mode=755
        install -m "$mode" "$work/${sources[index]}" "$stage/new"
        touch "$stage/ready"
    done
    for index in "${!destinations[@]}"; do
        mv -Tf "${stages[index]}/new" "${destinations[index]}"
    done
    committed=1
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database "$prefix/share/applications" || echo 'Warning: could not refresh the desktop database.' >&2
    fi
    if command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" || echo 'Warning: could not refresh the icon cache.' >&2
    fi
)

# Replacements require write/search permission on their parent directories.
# For missing directories, check the nearest existing ancestor instead.
needs_admin() {
    local directory
    for directory in "$PREFIX" "$PREFIX/bin" "$PREFIX/share/applications" \
        "$PREFIX/share/icons/hicolor/256x256/apps" "$PREFIX/share/icons/hicolor/scalable/apps"; do
        while [[ ! -e "$directory" ]]; do directory="$(dirname "$directory")"; done
        [[ -d "$directory" && -w "$directory" && -x "$directory" ]] || return 0
    done
    return 1
}

# SlintYT's in-app updater sets SLINTYT_ELEVATE=gui. There is no terminal for
# sudo then, so pkexec shows the desktop's own password prompt instead.
as_admin() {
    if [[ "${SLINTYT_ELEVATE:-}" == gui ]]; then
        command -v pkexec >/dev/null || {
            echo "pkexec (polkit) is needed to ask for your password. Run the install command in a terminal instead." >&2
            exit 1
        }
        local status=0
        pkexec "$@" || status=$?
        # pkexec uses 126 when the prompt is dismissed and 127 when it cannot ask,
        # for example because no polkit authentication agent is running.
        case "$status" in
            126) echo 'Permission was not granted.' >&2; exit 126 ;;
            127) echo 'Could not ask for your password. Start a polkit authentication agent, or run the install command in a terminal.' >&2; exit 1 ;;
        esac
        return "$status"
    fi
    command -v sudo >/dev/null || { echo "sudo is needed to write $PREFIX" >&2; exit 1; }
    sudo "$@"
}

# Until this point SlintYT's in-app updater may stop the installer, which is
# only downloading. It stops sending signals once it reads the "Installing to"
# line, so ignore them first: from here files may be replaced.
if [[ "${SLINTYT_ELEVATE:-}" == gui ]]; then
    trap '' HUP INT TERM
fi
echo "Installing to $PREFIX..."
if needs_admin; then
    echo "Administrator access is needed to write $PREFIX."
    as_admin "$(command -v bash)" -c "set -euo pipefail; $(declare -f install_files); install_files \"\$1\" \"\$2\"" \
        bash "$WORK_DIR" "$PREFIX"
else
    install_files "$WORK_DIR" "$PREFIX"
fi
if ! command -v yt-dlp >/dev/null; then
    echo 'Install yt-dlp and a supported JavaScript runtime (such as Deno) for YouTube playback.' >&2
fi
if ! { command -v ldconfig >/dev/null && ldconfig -p 2>/dev/null | grep 'libmpv\.so' >/dev/null; }; then
    echo 'Install libmpv if needed (libmpv2 on Ubuntu, mpv on Arch, mpv-libs on Fedora).' >&2
fi
printf 'Installed SlintYT under %s. Ensure %s/bin is on your desktop session PATH.\n' "$PREFIX" "$PREFIX"
