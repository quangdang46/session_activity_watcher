#!/usr/bin/env bash
set -euo pipefail
umask 022

BINARY_NAME="saw"
OWNER="quangdang46"
REPO="session_activity_watcher"
DEST="${DEST:-$HOME/.local/bin}"
VERSION="${VERSION:-}"
QUIET=0
EASY=0
VERIFY=0
FROM_SOURCE=0
UNINSTALL=0
MAX_RETRIES=3
DOWNLOAD_TIMEOUT=120
LOCK_DIR="/tmp/${BINARY_NAME}-install.lock.d"
TMP=""

log_info() {
    [ "$QUIET" -eq 1 ] && return 0
    echo "[${BINARY_NAME}] $*" >&2
}

log_warn() {
    echo "[${BINARY_NAME}] WARN: $*" >&2
}

log_success() {
    [ "$QUIET" -eq 1 ] && return 0
    echo "✓ $*" >&2
}

die() {
    echo "ERROR: $*" >&2
    exit 1
}

usage() {
    cat <<EOF
Install ${BINARY_NAME} from GitHub releases.

Usage:
  install.sh [options]

Options:
  --dest <dir>          Install into a custom directory
  --dest=<dir>          Install into a custom directory
  --version <tag>       Install a specific release tag
  --version=<tag>       Install a specific release tag
  --system              Install into /usr/local/bin
  --easy-mode           Append DEST to PATH in ~/.bashrc and ~/.zshrc
  --verify              Run ${BINARY_NAME} --version after install
  --from-source         Build from source instead of downloading a release artifact
  --quiet, -q           Reduce installer output
  --uninstall           Remove the installed binary and PATH helper lines
  -h, --help            Show this help text
EOF
    exit 0
}

cleanup() {
    rm -rf "$TMP" "$LOCK_DIR" 2>/dev/null || true
}

trap cleanup EXIT

acquire_lock() {
    if mkdir "$LOCK_DIR" 2>/dev/null; then
        echo $$ > "$LOCK_DIR/pid"
        return 0
    fi
    die "Another install is running. If stuck: rm -rf $LOCK_DIR"
}

remove_easy_mode_lines() {
    local rc="$1"
    local tmp_file

    [ -f "$rc" ] || return 0
    tmp_file="${rc}.tmp.$$"
    grep -vF "# ${BINARY_NAME} installer" "$rc" > "$tmp_file" 2>/dev/null || true
    mv -f "$tmp_file" "$rc" 2>/dev/null || rm -f "$tmp_file"
}

do_uninstall() {
    rm -f "$DEST/$BINARY_NAME" "$DEST/$BINARY_NAME.exe"
    for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
        remove_easy_mode_lines "$rc"
    done
    log_success "Uninstalled"
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            [ $# -ge 2 ] || die "missing value for --dest"
            DEST="$2"
            shift 2
            ;;
        --dest=*)
            DEST="${1#*=}"
            shift
            ;;
        --version)
            [ $# -ge 2 ] || die "missing value for --version"
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#*=}"
            shift
            ;;
        --system)
            DEST="/usr/local/bin"
            shift
            ;;
        --easy-mode)
            EASY=1
            shift
            ;;
        --verify)
            VERIFY=1
            shift
            ;;
        --from-source)
            FROM_SOURCE=1
            shift
            ;;
        --quiet|-q)
            QUIET=1
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

detect_platform() {
    local os arch
    case "$(uname -s)" in
        Linux*)
            os="linux"
            ;;
        Darwin*)
            os="macos"
            ;;
        MINGW*|MSYS*|CYGWIN*)
            os="windows"
            ;;
        *)
            die "Unsupported OS: $(uname -s)"
            ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64)
            arch="x86_64"
            ;;
        aarch64|arm64)
            arch="aarch64"
            ;;
        *)
            die "Unsupported arch: $(uname -m)"
            ;;
    esac
    echo "${os}_${arch}"
}

asset_suffix() {
    case "$1" in
        linux_x86_64) echo "linux-x86_64" ;;
        linux_aarch64) echo "linux-aarch64" ;;
        macos_x86_64) echo "macos-x86_64" ;;
        macos_aarch64) echo "macos-aarch64" ;;
        windows_x86_64) echo "windows-x86_64" ;;
        *) die "Unsupported platform: $1" ;;
    esac
}

archive_ext() {
    case "$1" in
        windows_x86_64) echo "zip" ;;
        *) echo "tar.gz" ;;
    esac
}

binary_filename() {
    case "$1" in
        windows_*) echo "${BINARY_NAME}.exe" ;;
        *) echo "$BINARY_NAME" ;;
    esac
}

resolve_version() {
    [ -n "$VERSION" ] && return 0

    VERSION=$(curl -fsSL \
        --connect-timeout 10 --max-time 30 \
        -H "Accept: application/vnd.github.v3+json" \
        "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" \
        2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/') || true

    if [ -z "$VERSION" ]; then
        VERSION=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
            "https://github.com/${OWNER}/${REPO}/releases/latest" \
            2>/dev/null | sed -E 's|.*/tag/||') || true
    fi

    [[ "$VERSION" =~ ^v[0-9] ]] || die "Could not resolve version"
    log_info "Latest release: $VERSION"
}

download_file() {
    local url="$1"
    local dest="$2"
    local partial="${dest}.part"
    local attempt=0
    local -a curl_args

    while [ $attempt -lt $MAX_RETRIES ]; do
        attempt=$((attempt + 1))
        curl_args=(
            -fL
            --connect-timeout 30
            --max-time "$DOWNLOAD_TIMEOUT"
            --retry 2
        )

        if [ -s "$partial" ]; then
            curl_args+=(--continue-at -)
        fi

        if [ "$QUIET" -eq 0 ] && [ -t 2 ]; then
            curl_args+=(--progress-bar)
        else
            curl_args+=(-sS)
        fi

        if curl "${curl_args[@]}" -o "$partial" "$url"; then
            mv -f "$partial" "$dest"
            return 0
        fi

        [ $attempt -lt $MAX_RETRIES ] && { log_warn "Retrying in 3s..."; sleep 3; }
    done

    rm -f "$partial"
    return 1
}

sha256_file() {
    local path="$1"

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
        return 0
    fi

    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$path" | awk '{print $1}'
        return 0
    fi

    die "No SHA-256 tool found"
}

verify_checksum() {
    local url="$1"
    local archive_path="$2"
    local checksum_path="$TMP/checksum.sha256"
    local expected actual

    if download_file "${url}.sha256" "$checksum_path" 2>/dev/null; then
        expected=$(awk '{print $1}' "$checksum_path")
        actual=$(sha256_file "$archive_path")
        [ "$expected" = "$actual" ] || die "Checksum mismatch"
    fi
}

extract_archive() {
    local archive_path="$1"

    case "$archive_path" in
        *.tar.gz)
            tar -xzf "$archive_path" -C "$TMP"
            ;;
        *.zip)
            if command -v unzip >/dev/null 2>&1; then
                unzip -q "$archive_path" -d "$TMP"
            else
                tar -xf "$archive_path" -C "$TMP"
            fi
            ;;
        *)
            die "Unsupported archive format: $archive_path"
            ;;
    esac
}

install_binary_atomic() {
    local src="$1"
    local dest="$2"
    local tmp="${dest}.tmp.$$"

    install -m 0755 "$src" "$tmp"
    mv -f "$tmp" "$dest" || { rm -f "$tmp"; die "Failed to install binary"; }
}

maybe_add_path() {
    case ":$PATH:" in
        *":$DEST:"*) return 0 ;;
    esac

    if [ "$EASY" -eq 1 ]; then
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
            [ -f "$rc" ] && [ -w "$rc" ] || continue
            grep -qF "$DEST" "$rc" && continue
            printf '\nexport PATH="%s:$PATH"  # %s installer\n' "$DEST" "$BINARY_NAME" >> "$rc"
        done
        log_warn "PATH updated — restart shell or: export PATH=\"$DEST:\$PATH\""
    else
        log_warn "Add to PATH: export PATH=\"$DEST:\$PATH\""
    fi
}

build_from_source() {
    local platform="$1"
    local binary_file
    local source_ref="main"

    command -v cargo >/dev/null || die "Rust/cargo not found. Install: https://rustup.rs"
    command -v git >/dev/null || die "git not found"

    [ -n "$VERSION" ] && source_ref="$VERSION"
    binary_file=$(binary_filename "$platform")

    git clone --depth 1 --branch "$source_ref" "https://github.com/${OWNER}/${REPO}.git" "$TMP/src"
    (
        cd "$TMP/src"
        CARGO_TARGET_DIR="$TMP/target" cargo build --release --locked -p "$BINARY_NAME" --bin "$BINARY_NAME"
    )
    install_binary_atomic "$TMP/target/release/$binary_file" "$DEST/$binary_file"
}

install_from_release() {
    local platform="$1"
    local suffix ext archive binary_file url

    suffix=$(asset_suffix "$platform")
    ext=$(archive_ext "$platform")
    archive="${BINARY_NAME}-${VERSION}-${suffix}.${ext}"
    binary_file=$(binary_filename "$platform")
    url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${archive}"

    if ! download_file "$url" "$TMP/$archive"; then
        log_warn "Binary download failed — building from source..."
        build_from_source "$platform"
        return 0
    fi

    verify_checksum "$url" "$TMP/$archive"
    extract_archive "$TMP/$archive"
    [ -f "$TMP/$binary_file" ] || die "Binary not found after extract"
    install_binary_atomic "$TMP/$binary_file" "$DEST/$binary_file"
}

print_summary() {
    local binary_path="$1"

    echo
    echo "✓ $(basename "$binary_path") installed → $binary_path"
    echo "  Version: $($binary_path --version 2>/dev/null || echo 'unknown')"
    echo
    echo "  Quick start:"
    echo "    ${BINARY_NAME} --help"
}

main() {
    local platform binary_file binary_path

    acquire_lock

    if [ "$UNINSTALL" -eq 1 ]; then
        do_uninstall
    fi

    mkdir -p "$DEST"
    TMP=$(mktemp -d)
    platform=$(detect_platform)
    binary_file=$(binary_filename "$platform")
    binary_path="$DEST/$binary_file"

    log_info "Platform: $platform | Dest: $DEST"

    if [ "$FROM_SOURCE" -eq 1 ]; then
        build_from_source "$platform"
    else
        resolve_version
        install_from_release "$platform"
    fi

    maybe_add_path

    [ "$VERIFY" -eq 1 ] && "$binary_path" --version >/dev/null

    print_summary "$binary_path"
}

if [[ "${BASH_SOURCE[0]:-}" == "${0:-}" ]] || [[ -z "${BASH_SOURCE[0]:-}" ]]; then
    { main "$@"; }
fi
