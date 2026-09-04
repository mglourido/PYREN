#!/bin/sh
# try-kernel-zones.sh - install omen-rgb-keyboard and find out whether the
# `kernelZones` dialect can answer on this machine (TODO §1.2b).
#
# Run as root:   sudo tools/try-kernel-zones.sh
# Undo it all:   sudo tools/try-kernel-zones.sh --rollback
#
# Two things this deliberately does NOT do, both of which the module's own
# README tells you to:
#
#   * it does not install the shipped `blacklist hp_wmi`, and
#   * it does not `modprobe -r hp_wmi` first.
#
# PYREN's entire fan control goes through hp-wmi's hwmon, and the ceiling
# we just measured (5200 rpm) came out of it. Removing hp_wmi to read one
# more RGB zone would be trading the project's main feature for a colour.
# So both modules are loaded at once - which the README calls a conflict -
# and the check below is the point of the whole script: if hp-wmi's pwm1
# stops answering, this rolls itself back.

set -eu

VERSION=1.4
NAME=omen-rgb-keyboard
SRC=/usr/src/$NAME-$VERSION
REPO=https://github.com/OmenLinux/omen-rgb-keyboard
CLONE=${CLONE:-/var/tmp/omen-rgb-keyboard}

[ "$(id -u)" = 0 ] || { echo "run me as root" >&2; exit 1; }

# hp-wmi's pwm1, looked up rather than assumed: the hwmon number is
# whatever order the machine happened to probe things in this boot, and it
# can differ before and after another module loads. Answers MISSING when
# there is no pwm1, which is the whole question this script asks twice.
pwm_now() {
    for candidate in /sys/devices/platform/hp-wmi/hwmon/hwmon*/pwm1; do
        [ -r "$candidate" ] && { cat "$candidate"; return; }
    done
    echo MISSING
}

rollback() {
    echo "--- rolling back"
    modprobe -r omen_rgb_keyboard 2>/dev/null || true
    dkms remove -m $NAME -v $VERSION --all 2>/dev/null || true
    rm -rf "$SRC"
    rm -f /etc/modules-load.d/$NAME.conf
    modprobe hp_wmi 2>/dev/null || true
    echo "--- gone. hp_wmi:"; lsmod | grep -c hp_wmi || true
}

[ "${1:-}" = "--rollback" ] && { rollback; exit 0; }

# What fan control looks like before we touch anything, to compare against.
before=$(pwm_now)
echo "--- before: hp-wmi pwm1 = $before"
[ "$before" = MISSING ] && { echo "hp-wmi hwmon is already gone; fix that first" >&2; exit 1; }

if [ ! -d "$CLONE" ]; then
    echo "--- cloning $REPO"
    git clone --depth 1 "$REPO" "$CLONE"
fi

echo "--- installing $NAME $VERSION through dkms"
rm -rf "$SRC"
cp -r "$CLONE" "$SRC"
dkms add -m $NAME -v $VERSION 2>/dev/null || true
dkms build -m $NAME -v $VERSION
dkms install -m $NAME -v $VERSION --force

echo "--- loading it (hp_wmi stays loaded)"
modprobe omen_rgb_keyboard

echo "--- loaded modules:"
lsmod | grep -E "omen_rgb_keyboard|hp_wmi" || echo "  neither?!"

# The check this script exists for.
after=$(pwm_now)
echo "--- after: hp-wmi pwm1 = $after"
if [ "$after" = MISSING ]; then
    echo "!!! hp-wmi's hwmon disappeared - fan control is gone. Rolling back."
    rollback
    exit 1
fi

echo "--- zone files:"
ls -l /sys/devices/platform/$NAME/rgb_zones/ 2>/dev/null || echo "  no rgb_zones under $NAME"
ls -l /sys/devices/platform/hp-wmi/rgb_zones/ 2>/dev/null || echo "  no rgb_zones under hp-wmi (expected)"

echo "--- zone colours, straight from sysfs:"
for z in 00 01 02 03; do
    f=/sys/devices/platform/$NAME/rgb_zones/zone$z
    [ -r "$f" ] && echo "  zone$z = $(cat "$f")" || echo "  zone$z unreadable"
done

echo
echo "--- done. Now restart the daemon and ask it:"
echo "      sudo systemctl restart pyren-daemon"
echo "      pyren-ctl rgb probe && pyren-ctl rgb read"
echo "    If any of this went wrong: sudo $0 --rollback"
