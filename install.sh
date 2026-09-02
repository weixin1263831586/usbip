#!/bin/sh
set -eu

install_dir="${USBIP_INSTALL_DIR:-/usr/local/bin}"

case "$(uname -m)" in
    x86_64|amd64)
        asset_name="usbipd-linux-x86_64"
        ;;
    *)
        echo "error: no prebuilt usbipd is available for $(uname -m)" >&2
        exit 1
        ;;
esac

if ! command -v curl >/dev/null 2>&1; then
    echo "error: curl is required" >&2
    exit 1
fi

build_dir="$(mktemp -d "${TMPDIR:-/tmp}/usbip-download.XXXXXX")"
trap 'rm -rf "$build_dir"' EXIT INT TERM

if [ -n "${USBIP_VERSION:-}" ]; then
    download_base="https://github.com/weixin1263831586/usbip/releases/download/$USBIP_VERSION"
else
    download_base="https://github.com/weixin1263831586/usbip/releases/latest/download"
fi

echo "Downloading prebuilt $asset_name..."
curl --proto '=https' --tlsv1.2 -fsSL \
    "$download_base/$asset_name" -o "$build_dir/$asset_name"
curl --proto '=https' --tlsv1.2 -fsSL \
    "$download_base/$asset_name.sha256" -o "$build_dir/$asset_name.sha256"
(cd "$build_dir" && sha256sum -c "$asset_name.sha256")

binary_path="$build_dir/$asset_name"

if [ "$(id -u)" -eq 0 ]; then
    install -D -m 0755 "$binary_path" "$install_dir/usbipd"
elif command -v sudo >/dev/null 2>&1; then
    sudo install -D -m 0755 "$binary_path" "$install_dir/usbipd"
else
    echo "error: installing to $install_dir requires root or sudo" >&2
    exit 1
fi

echo "Installed usbipd to $install_dir/usbipd"
"$install_dir/usbipd" --version
