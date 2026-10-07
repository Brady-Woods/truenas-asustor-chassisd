#!/bin/sh
# Idempotent build + deploy for lcm-status. Safe to re-run any time --
# every step either installs-if-missing or overwrites with the current
# source, nothing here depends on prior state beyond what it checks itself.
#
# Run as root on the TrueNAS SCALE box, from this directory:
#   sudo ./deploy.sh
#
# What "durable across TrueNAS upgrades" means here: TrueNAS SCALE updates
# land in a new ZFS boot environment (boot-pool/ROOT/<version>), and
# /usr -- and everything under it, including /usr/local -- is read-only by
# default on a fresh one (confirmed live on the first real update this
# project went through: an earlier version of this script tried to
# `install` the binary into /usr/local/sbin and failed with "Read-only
# file system"). So the binary runs straight out of this checkout instead
# (on a normal writable data-pool dataset), never installed anywhere.
# /etc *is* writable in a fresh boot environment (just not persisted
# across updates, which is exactly why this script re-installs the config/
# unit every time rather than treating a first install as special), and
# TrueNAS's own config database (which is what backs Init/Shutdown
# Scripts) is genuinely durable -- this script registers itself as a
# POSTINIT script there, so re-running it is exactly what happens on every
# boot, no manual re-deploy needed after an update.

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_PATH="$SCRIPT_DIR/target/release/lcm-status"
CONFIG_PATH=/etc/lcm-status.toml
UNIT_PATH=/etc/systemd/system/lcm-status.service
GROUP=lcm-status

log() { echo "==> $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }

# Escape a string for use inside a JSON string literal (backslash, double
# quote) so a path or name with those characters can't break out of the
# midclt arguments below.
json_escape() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'; }
# Escape a string for use as the replacement side of a sed s||| command
# (backslash, the | delimiter, and &).
sed_escape() { printf '%s' "$1" | sed 's/[\\|&]/\\&/g'; }

# --- 1. Kernel driver dependency check, before anything else -------------
#
# The LED functionality (bay LEDs, status LED) and the buzzer depend on the
# asustor-platform-driver fork, `main`, v0.3 or later
# (https://github.com/Brady-Woods/asustor-platform-driver), and front LED
# brightness on its vendored `it87` loaded with `led_pwm=3 led_pwm_invert=1`.
# Stock TrueNAS SCALE ships neither. Check for its actual effects (the kernel module,
# the asustor platform device, the LED class devices) rather than trusting
# a version string, since what matters is whether the sysfs interface this
# project uses actually exists.
check_driver() {
    log "Checking for asustor-platform-driver (fork main, v0.3 or later)..."
    # This and the platform driver's own Post Init script (which builds
    # and loads its modules from a checkout on the data pool) both run as
    # POSTINIT Init/Shutdown Scripts, and TrueNAS doesn't guarantee which
    # runs first. On a fresh boot environment the driver may still be
    # mid-rebuild when this starts, so poll instead of failing immediately.
    wait_secs="${DRIVER_WAIT_SECS:-60}"
    waited=0
    while ! lsmod | grep -q '^asustor_gpio_it87'; do
        [ "$waited" -ge "$wait_secs" ] && break
        sleep 1
        waited=$((waited + 1))
    done
    if ! lsmod | grep -q '^asustor_gpio_it87'; then
        fail "asustor_gpio_it87 kernel module not loaded (waited ${wait_secs}s).
  lcm-status's LED support (bay LEDs, status LED) and buzzer require the
  asustor-platform-driver fork, main, v0.3 or later:
    https://github.com/Brady-Woods/asustor-platform-driver
  Install that driver first, then re-run this script."
    fi
    # The asustor platform device's directory exists whenever the module is
    # loaded on a supported board. (/sys/class/leds/power:lcd, checked here
    # before, no longer exists: LCD power is now the driver's lcd_power.)
    [ -d /sys/devices/platform/asustor ] || fail "/sys/devices/platform/asustor not found -- the asustor module isn't loaded, or this board isn't supported. Need the asustor-platform-driver fork (https://github.com/Brady-Woods/asustor-platform-driver), main, v0.3 or later."
    for led in sata1:red:disk green:status red:status; do
        [ -d "/sys/class/leds/$led" ] || fail "expected LED class device /sys/class/leds/$led not found -- asustor-platform-driver may be an older or incomplete build. Need the asustor-platform-driver fork (https://github.com/Brady-Woods/asustor-platform-driver), main, v0.3 or later."
    done
    log "asustor-platform-driver OK (module loaded, platform device and expected LED class devices present)."
    # Only needed for [led] brightness / night_brightness, so not fatal.
    [ -d "/sys/class/leds/front_panel::brightness" ] || log "WARNING: /sys/class/leds/front_panel::brightness not found -- front LED brightness ([led] brightness/night_brightness) needs the fork's it87 loaded with led_pwm=3 led_pwm_invert=1."
}
check_driver

# --- 1b. LED trigger modules -----------------------------------------------
# ledtrig-timer (every blink pattern) and ledtrig-netdev ([led] nic_mode and
# night mode on the RTL8125 NIC LEDs) ship with the TrueNAS kernel but
# aren't loaded by default. The daemon loads them itself on every start
# (led::ensure_trigger_modules); doing it here as well just makes a missing
# module visible at deploy time rather than only in the journal.
for m in ledtrig-timer ledtrig-netdev; do
    modprobe "$m" || log "WARNING: modprobe $m failed -- LEDs that use it won't change"
done

# --- 2. Build, in a throwaway container -- nothing installed on the host -
#
# Only when the source changed since the last build (stamp next to the
# binary; target/ isn't synced over, so it stays on the NAS). At boot this
# runs as a Post Init script, before the Docker daemon is up (found live,
# 2026-10-06: "Cannot connect to the Docker daemon"), so with an unchanged
# checkout it mustn't need Docker at all; when a build is needed, wait for
# Docker a while, and if it never comes up keep the existing binary
# instead of failing the whole script (the group/unit/restart steps below
# still have to run).
STAMP_PATH="$SCRIPT_DIR/target/.build-stamp"
source_hash() {
    (cd "$SCRIPT_DIR" && find Cargo.toml Cargo.lock src -type f | LC_ALL=C sort |
        xargs sha256sum | sha256sum | cut -d' ' -f1)
}
want_stamp="$(source_hash)"
if [ -x "$BIN_PATH" ] && [ -f "$STAMP_PATH" ] && [ "$(cat "$STAMP_PATH")" = "$want_stamp" ]; then
    log "Binary already built from this source; skipping the build."
else
    docker_wait="${DOCKER_WAIT_SECS:-60}"
    waited=0
    until docker info >/dev/null 2>&1; do
        [ "$waited" -ge "$docker_wait" ] && break
        sleep 2
        waited=$((waited + 2))
    done
    if docker info >/dev/null 2>&1; then
        log "Building (containerized rust:alpine, musl target)..."
        # HOME: the docker CLI reads its config from there, and TrueNAS
        # runs Post Init scripts without it.
        HOME="${HOME:-/root}" docker run --rm \
            -v "$SCRIPT_DIR":/work -w /work \
            -e CARGO_HOME=/work/.cargo-home \
            rust:alpine \
            sh -c "apk add --no-cache musl-dev >/dev/null 2>&1 && cargo build --release"
        [ -x "$BIN_PATH" ] || fail "build did not produce target/release/lcm-status"
        echo "$want_stamp" > "$STAMP_PATH"
    elif [ -x "$BIN_PATH" ]; then
        log "WARNING: the source changed since the last build, but Docker isn't available (waited ${docker_wait}s) -- keeping the existing binary. Re-run deploy.sh once Docker is up to rebuild."
    else
        fail "no binary at $BIN_PATH and Docker isn't available (waited ${docker_wait}s) to build one"
    fi
fi

# --- 2b. Trust check on the checkout ---------------------------------------
#
# The systemd unit runs target/release/lcm-status as root straight out of
# this checkout, and this script is a root Post Init script, so anyone who
# can write to the checkout (or to a directory above it) can run code as
# root. Warn, don't fail: failing would stop the unit and the Post Init
# registration below on a box where the owner has accepted that.
check_trusted() {
    path="$1"
    [ -e "$path" ] || return 0
    info="$(stat -c '%u %A' "$path" 2>/dev/null)" || {
        log "WARNING: couldn't stat $path to check its ownership"
        return 0
    }
    owner="${info%% *}"
    perm="${info#* }"
    if [ "$owner" != 0 ]; then
        log "WARNING: $path is owned by uid $owner, not root -- root runs code from this checkout, so it should be root-owned."
    fi
    case "$perm" in
        ?????w????|????????w?)
            log "WARNING: $path is group- or world-writable ($perm) -- anyone who can write there can run code as root."
            ;;
    esac
}
log "Checking the checkout is root-owned and not writable by group/others..."
check_trusted "$SCRIPT_DIR/deploy.sh"
check_trusted "$SCRIPT_DIR/target"
check_trusted "$BIN_PATH"
check_trusted "$SCRIPT_DIR/src"
check_trusted "$SCRIPT_DIR/lcm-status.service"
trust_dir="$SCRIPT_DIR"
while :; do
    check_trusted "$trust_dir"
    [ "$trust_dir" = / ] && break
    trust_dir="$(dirname "$trust_dir")"
done

# lcm-status.service has ProtectHome=yes, which hides /home, /root and
# /run/user from the service: a checkout under them makes systemd fail the
# unit with status 203/EXEC. Warn only (never fail, see above).
check_not_in_home() {
    case "$1" in
        /home|/home/*|/root|/root/*)
            log "WARNING: the checkout is under $1, but the unit sets ProtectHome=yes, so the service will fail to start (203/EXEC). Move the checkout (e.g. under a dataset such as /mnt/<pool>/...) or remove ProtectHome from the unit."
            ;;
    esac
}
check_not_in_home "$SCRIPT_DIR"
physical_dir="$(cd "$SCRIPT_DIR" && pwd -P 2>/dev/null)" || physical_dir=""
[ "$physical_dir" = "$SCRIPT_DIR" ] || check_not_in_home "$physical_dir"

# --- 3. Group for socket access --------------------------------------------
#
# TrueNAS regenerates /etc/group from its config database at boot, so a
# group added with groupadd is gone after a reboot (and the daemon's chgrp of
# its socket fails: "invalid group"). Create it through the middleware so
# it's in that database; plain groupadd only where there's no middleware.
ensure_group() {
    if command -v midclt >/dev/null 2>&1; then
        json_group="$(json_escape "$GROUP")"
        existing="$(midclt call group.query "[[\"group\", \"=\", \"$json_group\"]]" 2>/dev/null || echo error)"
        if [ "$existing" = "[]" ]; then
            if midclt call group.create "{\"name\": \"$json_group\", \"smb\": false}" >/dev/null 2>&1; then
                log "Created group '$GROUP' in the TrueNAS config database (persists across reboots)."
            else
                log "WARNING: creating group '$GROUP' through the TrueNAS middleware failed; adding it to /etc/group only (lost at the next reboot)."
            fi
        elif [ "$existing" = error ]; then
            log "WARNING: couldn't query the TrueNAS middleware for group '$GROUP'."
        fi
    fi
    getent group "$GROUP" >/dev/null 2>&1 || groupadd -f "$GROUP"
    log "Group '$GROUP' present."
}
log "Ensuring group '$GROUP' exists..."
ensure_group

# --- 4. Binary needs no install step -- it runs straight from the build
# output in this checkout ($BIN_PATH); see the note above on why.

# --- 5. Install config, but never clobber an existing (possibly edited) one
if [ ! -f "$CONFIG_PATH" ]; then
    log "Installing default config to $CONFIG_PATH..."
    install -m 0644 "$SCRIPT_DIR/lcm-status.example.toml" "$CONFIG_PATH"
else
    log "Config already exists at $CONFIG_PATH, leaving it alone."
fi

# --- 6. systemd unit -------------------------------------------------------
# Substitute the real binary path (this checkout, see $BIN_PATH above) in
# place of the @LCM_STATUS_BIN@ placeholder the checked-in unit carries.
log "Installing systemd unit..."
sed "s|@LCM_STATUS_BIN@|$(sed_escape "$BIN_PATH")|" "$SCRIPT_DIR/lcm-status.service" > "$UNIT_PATH"
systemctl daemon-reload
systemctl enable lcm-status.service
systemctl restart lcm-status.service

# --- 7. Register this script as a POSTINIT task, so it self-heals --------
#
# after every boot, including into a fresh post-update boot environment
# that may not have any of the above. Idempotent: replaces any existing
# task pointing at this same script path rather than piling up duplicates.
log "Registering as a TrueNAS POSTINIT script (survives upgrades)..."
JSON_SCRIPT="$(json_escape "$SCRIPT_DIR/deploy.sh")"
EXISTING_ID=$(midclt call initshutdownscript.query \
    "[[\"script\", \"=\", \"$JSON_SCRIPT\"]]" 2>/dev/null \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[0]["id"]) if d else None' 2>/dev/null || true)

# 600 s: at boot a rebuild (after the checkout changed) waits for Docker
# and then compiles; the middleware kills a script that runs past its
# timeout (registered with 120 s before).
POSTINIT_TIMEOUT=600
if [ -n "${EXISTING_ID:-}" ] && [ "$EXISTING_ID" != "None" ]; then
    log "Existing POSTINIT task found (id=$EXISTING_ID), leaving it in place."
    midclt call initshutdownscript.update "$EXISTING_ID" "{\"timeout\": $POSTINIT_TIMEOUT}" >/dev/null 2>&1 ||
        log "WARNING: couldn't set the POSTINIT task's timeout to ${POSTINIT_TIMEOUT}s"
else
    midclt call initshutdownscript.create "{
        \"type\": \"SCRIPT\",
        \"script\": \"$JSON_SCRIPT\",
        \"when\": \"POSTINIT\",
        \"enabled\": true,
        \"timeout\": $POSTINIT_TIMEOUT
    }" >/dev/null
    log "POSTINIT task registered."
fi

log "Done. lcm-status is running:"
systemctl status lcm-status.service --no-pager | head -5
