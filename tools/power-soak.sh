#!/bin/sh
# power-soak.sh - exercise the four performance profiles on this machine.
#
# The hermetic suite (`cargo test -p pyren-power --test profiles`) drives
# a fake laptop, which is what makes it able to assert exact values and
# run anywhere. This is the other half: the real daemon, the real
# firmware, the real power-profiles-daemon, and the two lifecycle
# questions a fixture cannot honestly answer -
#
#   * what happens to the profile when the app is closed, and
#   * what happens when the daemon itself is restarted.
#
# Nothing here is destructive beyond what clicking the modes in the app
# does, and it puts the machine back where it found it on the way out -
# including on Ctrl-C.
#
#   tools/power-soak.sh                  the full run (about 6 minutes)
#   tools/power-soak.sh --minutes 15     watch it evolve for longer
#   tools/power-soak.sh --quick          switching only, no waiting
#
# Needs a running pyren-daemon and pyren-ctl on PATH (or built in the
# tree, which is where this looks first).

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
MINUTES=5
QUICK=no
FAILURES=0
CHECKS=0

while [ $# -gt 0 ]; do
    case "$1" in
        --minutes) MINUTES=$2; shift 2 ;;
        --quick) QUICK=yes; MINUTES=0; shift ;;
        -h | --help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "power-soak.sh: unknown argument '$1' (try --help)" >&2; exit 2 ;;
    esac
done

CTL=$(command -v pyren-ctl 2>/dev/null || true)
[ -n "$CTL" ] || CTL="$ROOT/daemon/target/debug/pyren-ctl"
[ -x "$CTL" ] || { echo "power-soak.sh: no pyren-ctl (build it: cd daemon && cargo build)" >&2; exit 1; }

bold() { printf '\033[1m==> %s\033[0m\n' "$1"; }
note() { printf '    %s\n' "$1"; }

# A check is a claim about the machine, with the evidence printed either
# way: a soak that only says "ok" is one nobody can debug at 2am.
check() {
    CHECKS=$((CHECKS + 1))
    if [ "$2" = "$3" ]; then
        printf '    \033[32mok\033[0m   %s (%s)\n' "$1" "$3"
    else
        FAILURES=$((FAILURES + 1))
        printf '    \033[31mFAIL\033[0m %s: expected %s, got %s\n' "$1" "$3" "$2"
    fi
}

state() { "$CTL" power get --json; }
field() { state | python3 -c "import json,sys;print(json.load(sys.stdin).get('$1',''))"; }
mode_now() { field mode; }

# What the machine itself says, read straight from the kernel rather than
# from the daemon - the whole question is whether the two agree.
hardware_profile() {
    if [ -r /sys/firmware/acpi/platform_profile ]; then
        cat /sys/firmware/acpi/platform_profile
    else
        echo none
    fi
}

os_profile() {
    if command -v powerprofilesctl >/dev/null 2>&1; then
        powerprofilesctl get
    else
        echo none
    fi
}

report() {
    printf '    mode=%-12s firmware=%-12s os=%s\n' "$(mode_now)" "$(hardware_profile)" "$(os_profile)"
}

# --- put it back on the way out, however we leave -------------------
STARTING_MODE=$(mode_now 2>/dev/null || echo balanced)
restore() {
    printf '\n'
    bold "restoring $STARTING_MODE"
    "$CTL" power set "$STARTING_MODE" >/dev/null 2>&1 || true
}
trap restore EXIT INT TERM

bold "the daemon"
"$CTL" power get >/dev/null || { echo "power-soak.sh: the daemon is not answering" >&2; exit 1; }
note "starting mode: $STARTING_MODE"
report

# --- 1. each of the four --------------------------------------------
#
# What each mode should leave the firmware profile reading depends on
# which names this firmware offers, so it is asked rather than assumed.
CHOICES=$(cat /sys/firmware/acpi/platform_profile_choices 2>/dev/null || echo "")
note "firmware offers: ${CHOICES:-nothing}"

expected_firmware() {
    # The same preference order as backend::pick_platform_profile.
    case "$1" in
        eco) wanted="low-power quiet cool balanced" ;;
        balanced) wanted="balanced balanced-performance quiet" ;;
        performance) wanted="balanced-performance performance balanced" ;;
        unlimited) wanted="performance balanced-performance" ;;
    esac
    for name in $wanted; do
        for offered in $CHOICES; do
            [ "$name" = "$offered" ] && { echo "$name"; return; }
        done
    done
    echo none
}

expected_os() {
    case "$1" in
        eco) echo power-saver ;;
        balanced) echo balanced ;;
        performance | unlimited) echo performance ;;
    esac
}

bold "switching between the four"
for mode in eco balanced performance unlimited; do
    "$CTL" power set "$mode" >/dev/null
    sleep 1
    check "$mode: the daemon" "$(mode_now)" "$mode"
    if [ "$CHOICES" != "" ]; then
        check "$mode: the laptop's own profile" "$(hardware_profile)" "$(expected_firmware "$mode")"
    fi
    if [ "$(os_profile)" != none ]; then
        check "$mode: the OS profile" "$(os_profile)" "$(expected_os "$mode")"
    fi
done

bold "cycling them ten times over"
i=0
while [ $i -lt 10 ]; do
    for mode in eco balanced performance unlimited; do
        "$CTL" power set "$mode" >/dev/null
    done
    i=$((i + 1))
done
sleep 1
check "ten rounds end where the last one left it" "$(mode_now)" "unlimited"
check "and the machine is there too" "$(hardware_profile)" "$(expected_firmware unlimited)"

# --- 2. closing the app ---------------------------------------------
#
# The app is a client. Closing its window - in front, in the background
# or minimised to the tray, which are the same thing to everything below
# the window manager - closes a socket. The mode belongs to the daemon,
# which is still running.
bold "closing the app"
"$CTL" power set eco >/dev/null
sleep 1
BEFORE_FIRMWARE=$(hardware_profile)
BEFORE_OS=$(os_profile)

if pgrep -x pyren >/dev/null 2>&1; then
    note "pyren is running; closing it"
    pkill -x pyren || true
    sleep 3
    check "the app is gone" "$(pgrep -x pyren >/dev/null 2>&1 && echo running || echo gone)" "gone"
else
    note "pyren is not running - this is the same state it leaves behind"
fi

check "the daemon still says Eco" "$(mode_now)" "eco"
check "the laptop's own profile did not move" "$(hardware_profile)" "$BEFORE_FIRMWARE"
check "the OS profile did not move" "$(os_profile)" "$BEFORE_OS"

# --- 3. minutes of it -----------------------------------------------
if [ "$QUICK" = no ]; then
    bold "$MINUTES minutes with nobody connected"
    note "sampling every 15s; the mode should only move if auto-switching is on"
    AUTO=$(state | python3 -c "import json,sys;print(json.load(sys.stdin)['auto']['enabled'])")
    note "auto-switching: $AUTO"

    START=$(mode_now)
    SWITCHES=0
    LAST=$START
    ELAPSED=0
    TOTAL=$((MINUTES * 60))
    while [ $ELAPSED -lt $TOTAL ]; do
        sleep 15
        ELAPSED=$((ELAPSED + 15))
        NOW=$(mode_now)
        if [ "$NOW" != "$LAST" ]; then
            SWITCHES=$((SWITCHES + 1))
            printf '    %4ss  switched %s -> %s\n' "$ELAPSED" "$LAST" "$NOW"
            LAST=$NOW
        fi
        # The claim that has to hold at every sample: the daemon reports
        # the mode the machine is actually in.
        if [ "$CHOICES" != "" ] && [ "$(hardware_profile)" != "$(expected_firmware "$NOW")" ]; then
            FAILURES=$((FAILURES + 1))
            printf '    \033[31mFAIL\033[0m at %ss: daemon says %s, firmware says %s\n' \
                "$ELAPSED" "$NOW" "$(hardware_profile)"
        fi
        CHECKS=$((CHECKS + 1))
    done

    if [ "$AUTO" = "False" ]; then
        check "auto is off, so nothing should have moved" "$SWITCHES" "0"
    else
        note "$SWITCHES switches in $MINUTES minutes"
        [ "$SWITCHES" -le "$MINUTES" ] ||
            { FAILURES=$((FAILURES + 1)); printf '    \033[31mFAIL\033[0m more than one switch a minute is flapping\n'; }
        CHECKS=$((CHECKS + 1))
    fi
fi

# --- 4. restarting the daemon ---------------------------------------
#
# The one event that can lose the mode: everything in memory goes, and
# only what reached power.json comes back - and only if the user ticked
# "restore on start".
bold "restarting the daemon"
UNIT=pyren-daemon.service
if systemctl cat "$UNIT" >/dev/null 2>&1; then
    RESTORE=$(state | python3 -c "import json,sys;print(json.load(sys.stdin)['restoreModeOnStart'])")
    note "restoreModeOnStart: $RESTORE"

    "$CTL" power set eco >/dev/null
    sleep 1
    BEFORE_FIRMWARE=$(hardware_profile)

    # pkexec pops a graphical polkit prompt rather than reading a
    # password from this terminal, which is the only kind of prompt a
    # session with no attached tty (this one, or an agent's) can actually
    # answer. Falls back to sudo when there is no graphical session to
    # pop a dialog in - a bare console over ssh, say.
    if command -v pkexec >/dev/null 2>&1 && [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
        note "pkexec systemctl restart $UNIT (watch for the auth dialog)"
        pkexec systemctl restart "$UNIT"
    else
        note "sudo systemctl restart $UNIT"
        sudo systemctl restart "$UNIT"
    fi
    sleep 3
    "$CTL" power get >/dev/null || { echo "    the daemon did not come back" >&2; exit 1; }

    # Whichever way the setting is set, the firmware profile must not
    # have been changed *by the restart itself*: with restore off the
    # daemon leaves the machine alone, and with it on it re-applies the
    # same mode, which lands on the same profile.
    check "the machine is where it was before the restart" "$(hardware_profile)" "$BEFORE_FIRMWARE"
    if [ "$RESTORE" = "True" ]; then
        check "restore-on-start brought Eco back" "$(mode_now)" "eco"
    else
        note "restore is off; the daemon reports what it found: $(mode_now)"
    fi
else
    note "no $UNIT installed - skipping (start your daemon under systemd to test this)"
fi

# --- verdict ---------------------------------------------------------
printf '\n'
if [ "$FAILURES" -eq 0 ]; then
    bold "$CHECKS checks, all passed"
else
    bold "$CHECKS checks, $FAILURES FAILED"
fi
exit $([ "$FAILURES" -eq 0 ] && echo 0 || echo 1)
