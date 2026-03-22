#!/usr/bin/env bash
# install-service.sh — Install HomeCore as a system daemon (Linux/macOS).
#
# Supports:
#   Linux  — systemd (systemctl)
#   macOS  — launchd (launchctl)
#
# Usage:
#   ./scripts/install-service.sh [OPTIONS]
#
# Options:
#   --install-dir DIR   HomeCore installation directory
#                       (default: /opt/homecore on Linux, ~/homecore on macOS)
#   --user USER         Run service as this user (Linux only; default: current user)
#   --system            Install as a system-level daemon (macOS: LaunchDaemons,
#                       requires root; Linux: /etc/systemd, default behaviour)
#   --user-agent        Install as a user-level agent (macOS: ~/Library/LaunchAgents;
#                       Linux: ~/.config/systemd/user/)
#   --uninstall         Remove the installed service
#   --status            Show current service status
#   --help              Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMPLATE_DIR="$SCRIPT_DIR/service-templates"
SERVICE_NAME="homecore"
PLIST_ID="com.homecore.homecore"

# ---------------------------------------------------------------------------
# Defaults (overridden by flags / OS detection below)
# ---------------------------------------------------------------------------
INSTALL_DIR=""
RUN_USER="$(id -un)"
RUN_GROUP="$(id -gn)"
USER_LEVEL=false
ACTION="install"

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --install-dir)  INSTALL_DIR="$2"; shift 2 ;;
        --user)         RUN_USER="$2"; RUN_GROUP="$2"; shift 2 ;;
        --system)       USER_LEVEL=false; shift ;;
        --user-agent)   USER_LEVEL=true; shift ;;
        --uninstall)    ACTION="uninstall"; shift ;;
        --status)       ACTION="status"; shift ;;
        --help|-h)
            sed -n '2,/^set /p' "$0" | grep -E '^#' | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# ---------------------------------------------------------------------------
# OS detection
# ---------------------------------------------------------------------------
OS="$(uname -s)"
case "$OS" in
    Linux)  PLATFORM="linux" ;;
    Darwin) PLATFORM="macos" ;;
    *)
        echo "Unsupported platform: $OS" >&2
        echo "Use install-service.ps1 on Windows." >&2
        exit 1
        ;;
esac

# ---------------------------------------------------------------------------
# Default install directory
# ---------------------------------------------------------------------------
if [[ -z "$INSTALL_DIR" ]]; then
    if [[ "$PLATFORM" == "linux" ]]; then
        INSTALL_DIR="/opt/homecore"
    else
        INSTALL_DIR="$HOME/homecore"
    fi
fi

INSTALL_DIR="$(realpath "$INSTALL_DIR" 2>/dev/null || echo "$INSTALL_DIR")"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()  { echo "==> $*"; }
info() { echo "    $*"; }
warn() { echo "    WARN: $*" >&2; }
die()  { echo "ERROR: $*" >&2; exit 1; }

require_root() {
    [[ "$(id -u)" -eq 0 ]] || die "This operation requires root. Re-run with sudo."
}

fill_template() {
    local template="$1"
    sed \
        -e "s|@@INSTALL_DIR@@|$INSTALL_DIR|g" \
        -e "s|@@USER@@|$RUN_USER|g" \
        -e "s|@@GROUP@@|$RUN_GROUP|g" \
        "$template"
}

# ---------------------------------------------------------------------------
# Linux — systemd
# ---------------------------------------------------------------------------
linux_unit_path() {
    if $USER_LEVEL; then
        echo "$HOME/.config/systemd/user/${SERVICE_NAME}.service"
    else
        echo "/etc/systemd/system/${SERVICE_NAME}.service"
    fi
}

linux_install() {
    $USER_LEVEL || require_root
    local unit_path
    unit_path="$(linux_unit_path)"

    log "Installing systemd unit → $unit_path"
    info "Install dir : $INSTALL_DIR"
    info "Run as user : $RUN_USER"
    info "User-level  : $USER_LEVEL"

    [[ -f "$TEMPLATE_DIR/homecore.service" ]] || \
        die "Template not found: $TEMPLATE_DIR/homecore.service"

    mkdir -p "$(dirname "$unit_path")"
    fill_template "$TEMPLATE_DIR/homecore.service" > "$unit_path"

    if $USER_LEVEL; then
        systemctl --user daemon-reload
        systemctl --user enable "$SERVICE_NAME"
        log "Enabled user service. To start:"
        info "systemctl --user start $SERVICE_NAME"
        info "systemctl --user status $SERVICE_NAME"
        info "journalctl --user -u $SERVICE_NAME -f"
    else
        systemctl daemon-reload
        systemctl enable "$SERVICE_NAME"
        log "Enabled system service. To start:"
        info "sudo systemctl start $SERVICE_NAME"
        info "sudo systemctl status $SERVICE_NAME"
        info "journalctl -u $SERVICE_NAME -f"
    fi
}

linux_uninstall() {
    $USER_LEVEL || require_root
    local unit_path
    unit_path="$(linux_unit_path)"

    if $USER_LEVEL; then
        systemctl --user stop "$SERVICE_NAME"   2>/dev/null || true
        systemctl --user disable "$SERVICE_NAME" 2>/dev/null || true
    else
        systemctl stop "$SERVICE_NAME"    2>/dev/null || true
        systemctl disable "$SERVICE_NAME" 2>/dev/null || true
    fi

    if [[ -f "$unit_path" ]]; then
        rm "$unit_path"
        log "Removed $unit_path"
        $USER_LEVEL && systemctl --user daemon-reload || systemctl daemon-reload
    else
        warn "Unit file not found: $unit_path"
    fi
}

linux_status() {
    if $USER_LEVEL; then
        systemctl --user status "$SERVICE_NAME" || true
    else
        systemctl status "$SERVICE_NAME" || true
    fi
}

# ---------------------------------------------------------------------------
# macOS — launchd
# ---------------------------------------------------------------------------
macos_plist_path() {
    if $USER_LEVEL; then
        echo "$HOME/Library/LaunchAgents/${PLIST_ID}.plist"
    else
        echo "/Library/LaunchDaemons/${PLIST_ID}.plist"
    fi
}

macos_install() {
    $USER_LEVEL || require_root
    local plist_path
    plist_path="$(macos_plist_path)"

    log "Installing launchd plist → $plist_path"
    info "Install dir : $INSTALL_DIR"
    info "User-level  : $USER_LEVEL"

    [[ -f "$TEMPLATE_DIR/com.homecore.homecore.plist" ]] || \
        die "Template not found: $TEMPLATE_DIR/com.homecore.homecore.plist"

    mkdir -p "$(dirname "$plist_path")"
    fill_template "$TEMPLATE_DIR/com.homecore.homecore.plist" > "$plist_path"

    # Unload first if already loaded (ignore errors on first install)
    launchctl unload "$plist_path" 2>/dev/null || true
    launchctl load -w "$plist_path"

    log "Service installed and loaded."
    if $USER_LEVEL; then
        info "launchctl list $PLIST_ID"
        info "tail -f $INSTALL_DIR/logs/homecore-stderr.log"
        info ""
        info "To stop:    launchctl unload $plist_path"
        info "To disable: launchctl unload -w $plist_path"
    else
        info "sudo launchctl list $PLIST_ID"
        info "sudo tail -f $INSTALL_DIR/logs/homecore-stderr.log"
        info ""
        info "To stop:    sudo launchctl unload $plist_path"
        info "To disable: sudo launchctl unload -w $plist_path"
    fi
}

macos_uninstall() {
    $USER_LEVEL || require_root
    local plist_path
    plist_path="$(macos_plist_path)"

    if [[ -f "$plist_path" ]]; then
        launchctl unload -w "$plist_path" 2>/dev/null || true
        rm "$plist_path"
        log "Removed $plist_path"
    else
        warn "Plist not found: $plist_path"
    fi
}

macos_status() {
    local plist_path
    plist_path="$(macos_plist_path)"

    echo "Plist: $plist_path"
    if [[ -f "$plist_path" ]]; then
        echo "File : present"
    else
        echo "File : not installed"
    fi
    echo
    launchctl list "$PLIST_ID" 2>/dev/null || echo "(not loaded)"
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------
case "$ACTION" in
    install)
        case "$PLATFORM" in
            linux) linux_install ;;
            macos) macos_install ;;
        esac
        ;;
    uninstall)
        case "$PLATFORM" in
            linux) linux_uninstall ;;
            macos) macos_uninstall ;;
        esac
        ;;
    status)
        case "$PLATFORM" in
            linux) linux_status ;;
            macos) macos_status ;;
        esac
        ;;
esac
