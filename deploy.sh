#!/bin/sh
# Idempotent build + deploy for lcm-status. Safe to re-run any time --
# every step either installs-if-missing or overwrites with the current
# source, nothing here depends on prior state beyond what it checks itself.
#
# Run as root on the TrueNAS SCALE box, from this directory:
#   sudo ./deploy.sh
#
# What "durable across TrueNAS upgrades" means here: TrueNAS SCALE updates
# land in a new ZFS boot environment (boot-pool/ROOT/<version>), and there
# is no guarantee files placed directly under /usr/local or
# /etc/systemd/system on the current one survive into the next -- that
# depends on update internals this script has no business assuming. What
# IS guaranteed to survive is TrueNAS's own config database, so this
# script registers itself as a POSTINIT Init/Shutdown Script (System
# Settings -> Advanced in the UI, or `midclt call initshutdownscript.query`)
# and re-running it is exactly what that does on every boot -- so even a
# boot environment that lost /usr/local entirely self-heals on the next
# start, no manual re-deploy needed after an update.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_PATH=/usr/local/sbin/lcm-status
CONFIG_PATH=/etc/lcm-status.toml
UNIT_PATH=/etc/systemd/system/lcm-status.service
GROUP=lcm-status

log() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }

# --- 1. Kernel driver dependency check, before anything else -------------
#
# The LED functionality (bay LEDs, status LED, LCD power) depends on
# mafredri/asustor-platform-driver's `nas-deploy` branch (main + PRs #46,
# #47, #48: https://github.com/mafredri/asustor-platform-driver/pulls).
# Stock TrueNAS SCALE does not ship this. Check for its actual effects
# (the kernel module + the LED class devices it creates) rather than
# trusting a version string, since what matters is whether the sysfs
# interface this project writes to actually exists.
check_driver() {
    log "Checking for asustor-platform-driver (nas-deploy branch)..."
    if ! lsmod | grep -q '^asustor_gpio_it87'; then
        fail "asustor_gpio_it87 kernel module not loaded.
  lcm-status's LED support (bay LEDs, status LED, LCD power/sleep) requires
  mafredri/asustor-platform-driver, branch nas-deploy (main + PRs #46, #47, #48):
    https://github.com/mafredri/asustor-platform-driver/pulls
  Install that driver first, then re-run this script."
    fi
    for led in power:lcd sata1:red:disk green:status red:status; do
        [ -d "/sys/class/leds/$led" ] || fail "expected LED class device /sys/class/leds/$led not found -- asustor-platform-driver may be an older or incomplete build. Need the nas-deploy branch (main + PRs #46, #47, #48)."
    done
    log "asustor-platform-driver OK (module loaded, expected LED class devices present)."
}
check_driver

# --- 2. Build, in a throwaway container -- nothing installed on the host -
log "Building (containerized rust:alpine, musl target)..."
docker run --rm \
    -v "$SCRIPT_DIR":/work -w /work \
    -e CARGO_HOME=/work/.cargo-home \
    rust:alpine \
    sh -c "apk add --no-cache musl-dev >/dev/null 2>&1 && cargo build --release"
[ -x "$SCRIPT_DIR/target/release/lcm-status" ] || fail "build did not produce target/release/lcm-status"

# --- 3. Group for socket access (idempotent: -f skips if it exists) ------
log "Ensuring group '$GROUP' exists..."
groupadd -f "$GROUP"

# --- 4. Install binary -----------------------------------------------------
log "Installing binary to $BIN_PATH..."
install -m 0755 "$SCRIPT_DIR/target/release/lcm-status" "$BIN_PATH"

# --- 5. Install config, but never clobber an existing (possibly edited) one
if [ ! -f "$CONFIG_PATH" ]; then
    log "Installing default config to $CONFIG_PATH..."
    install -m 0644 "$SCRIPT_DIR/lcm-status.example.toml" "$CONFIG_PATH"
else
    log "Config already exists at $CONFIG_PATH, leaving it alone."
fi

# --- 6. systemd unit -------------------------------------------------------
log "Installing systemd unit..."
install -m 0644 "$SCRIPT_DIR/lcm-status.service" "$UNIT_PATH"
systemctl daemon-reload
systemctl enable lcm-status.service
systemctl restart lcm-status.service

# --- 7. Register this script as a POSTINIT task, so it self-heals --------
#
# after every boot, including into a fresh post-update boot environment
# that may not have any of the above. Idempotent: replaces any existing
# task pointing at this same script path rather than piling up duplicates.
log "Registering as a TrueNAS POSTINIT script (survives upgrades)..."
EXISTING_ID=$(midclt call initshutdownscript.query \
    "[[\"script\", \"=\", \"$SCRIPT_DIR/deploy.sh\"]]" 2>/dev/null \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[0]["id"]) if d else None' 2>/dev/null || true)

if [ -n "${EXISTING_ID:-}" ] && [ "$EXISTING_ID" != "None" ]; then
    log "Existing POSTINIT task found (id=$EXISTING_ID), leaving it in place."
else
    midclt call initshutdownscript.create "{
        \"type\": \"SCRIPT\",
        \"script\": \"$SCRIPT_DIR/deploy.sh\",
        \"when\": \"POSTINIT\",
        \"enabled\": true,
        \"timeout\": 120
    }" >/dev/null
    log "POSTINIT task registered."
fi

log "Done. lcm-status is running:"
systemctl status lcm-status.service --no-pager | head -5
