#!/usr/bin/env bash
set -Eeuo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_dir="$(cd -- "$script_dir/.." && pwd)"

host_bin="${USBIP_HOST_BIN:-$project_dir/target/release/usbipd}"
vid="${USBIP_VID:-}"
pid="${USBIP_PID:-}"
serial="${USBIP_SERIAL:-}"
listen="${USBIP_LISTEN:-0.0.0.0:3240}"
stop_adb=0
use_sudo=1

usage() {
    cat <<'EOF'
Usage: scripts/usbip-host.sh [options]

Options:
  --vid HEX          USB vendor ID (or set USBIP_VID)
  --pid HEX          USB product ID (or set USBIP_PID)
  --serial SERIAL    USB serial number (or set USBIP_SERIAL)
  --listen ADDR      Listen address (default: USBIP_LISTEN or 0.0.0.0:3240)
  --stop-adb         Run "adb kill-server" before starting the host
  --no-sudo          Run the host as the current user (requires USB permissions)
  --help             Show this help

Environment:
  USBIP_HOST_BIN     Path to the release usbipd binary
  USBIP_VID          Default vendor ID
  USBIP_PID          Default product ID
  USBIP_SERIAL       Default serial number
  USBIP_LISTEN       Default listen address
EOF
}

while (($# > 0)); do
    case "$1" in
        --vid)
            (($# >= 2)) || { echo "missing value for --vid" >&2; exit 2; }
            vid="$2"
            shift 2
            ;;
        --pid)
            (($# >= 2)) || { echo "missing value for --pid" >&2; exit 2; }
            pid="$2"
            shift 2
            ;;
        --serial)
            (($# >= 2)) || { echo "missing value for --serial" >&2; exit 2; }
            serial="$2"
            shift 2
            ;;
        --listen)
            (($# >= 2)) || { echo "missing value for --listen" >&2; exit 2; }
            listen="$2"
            shift 2
            ;;
        --stop-adb)
            stop_adb=1
            shift
            ;;
        --no-sudo)
            use_sudo=0
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -z "$vid" || -z "$pid" || -z "$serial" ]]; then
    echo "--vid, --pid, and --serial are required" >&2
    usage >&2
    exit 2
fi

if [[ ! -x "$host_bin" ]]; then
    echo "host binary not found or not executable: $host_bin" >&2
    echo "build it with: cargo build --release --bin usbipd" >&2
    exit 1
fi

if ((stop_adb)); then
    if ! command -v adb >/dev/null 2>&1; then
        echo 'warning: adb not found; continuing without stopping ADB' >&2
    elif ! adb kill-server; then
        echo 'warning: adb kill-server failed; continuing anyway' >&2
    fi
fi

if ((use_sudo)); then
    cmd=(sudo "$host_bin" bind --vid "$vid" --pid "$pid" --serial "$serial" --listen "$listen")
else
    cmd=("$host_bin" bind --vid "$vid" --pid "$pid" --serial "$serial" --listen "$listen")
fi

exec "${cmd[@]}"
