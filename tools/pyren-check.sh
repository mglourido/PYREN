#!/bin/sh
# pyren-check.sh - verify that fan control works on this machine.
#
# Portable stand-in for `pyren-check` (daemon/check), for when building
# the project isn't practical: copy this one file to the laptop and run it.
# POSIX sh, no dependencies beyond coreutils.
#
# It performs the same checks, in the same order, with the same verdicts and
# exit codes as the Rust version. `daemon/check/tests/parity.rs` compares
# the two against fixtures so they cannot drift apart silently.
#
#   ./pyren-check.sh            read-only, safe on any machine
#   sudo ./pyren-check.sh -w    also verify the PWM accepts writes
#   ./pyren-check.sh --json     machine-readable, for bug reports
#
# Exit status: 0 full control, 1 monitoring only, 2 no interface.

set -u

HP_WMI_DIR="/sys/devices/platform/hp-wmi"
ALLOW_WRITES=0
AS_JSON=0

usage() {
	cat <<'USAGE'
pyren-check.sh - verify that fan control works on this machine

USAGE:
    pyren-check.sh [OPTIONS]

OPTIONS:
    -w, --write   Also verify that the PWM channel accepts writes. Rewrites
                  the value already set and restores the previous mode, so
                  no fan changes speed. Needs root.
    -j, --json    Print the report as JSON.
    -h, --help    Show this help.

EXIT STATUS:
    0  fan control works
    1  fan speeds can be read but not set
    2  no HP fan-control interface on this machine
USAGE
}

for arg in "$@"; do
	case "$arg" in
	-w | --write) ALLOW_WRITES=1 ;;
	-j | --json) AS_JSON=1 ;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "pyren-check.sh: unknown argument '$arg'" >&2
		usage >&2
		exit 64
		;;
	esac
done

CHECKS="$(mktemp)" || exit 70
trap 'rm -f "$CHECKS"' EXIT INT TERM

# record <status> <id> <title> <detail> [remedy]
# Fields are tab-separated, so details may contain spaces.
record() {
	printf '%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "${5:-}" >>"$CHECKS"
}

status_of() {
	awk -F'\t' -v id="$1" '$2 == id { print $1; exit }' "$CHECKS"
}

read_value() {
	[ -r "$1" ] || return 1
	# tr strips the trailing newline sysfs always writes.
	tr -d '\n\r' <"$1"
}

is_number() {
	case "$1" in
	'' | *[!0-9]*) return 1 ;;
	*) return 0 ;;
	esac
}

# --- discovery ---------------------------------------------------------

# PYREN_HWMON_DIR points the checks at a fixture, matching the Rust
# version, so this can be exercised without HP hardware.
HWMON=""
if [ -n "${PYREN_HWMON_DIR:-}" ]; then
	[ -d "$PYREN_HWMON_DIR" ] && HWMON="$PYREN_HWMON_DIR"
else
	for candidate in "$HP_WMI_DIR"/hwmon/*/; do
		[ -d "$candidate" ] || continue
		HWMON="${candidate%/}"
		break
	done
fi

CPU_TEMP=""
for chip in /sys/class/hwmon/hwmon*/; do
	[ -r "${chip}name" ] || continue
	name="$(read_value "${chip}name" 2>/dev/null)" || continue
	case "$name" in
	coretemp | k10temp)
		[ -r "${chip}temp1_input" ] && CPU_TEMP="${chip}temp1_input" && break
		;;
	esac
done
if [ -z "$CPU_TEMP" ] && [ -r /sys/class/thermal/thermal_zone0/temp ]; then
	CPU_TEMP="/sys/class/thermal/thermal_zone0/temp"
fi

# --- checks ------------------------------------------------------------

if [ -d "$HP_WMI_DIR" ]; then
	record pass hp-wmi "hp-wmi platform driver" "$HP_WMI_DIR is present"
	HAS_HP_WMI=1
else
	record fail hp-wmi "hp-wmi platform driver" "$HP_WMI_DIR does not exist" \
		"This is normal on non-HP hardware. On an HP laptop, check that the hp_wmi module is loaded (modprobe hp_wmi) and that the BIOS exposes WMI."
	HAS_HP_WMI=0
fi

if [ -n "$HWMON" ]; then
	record pass hwmon "hwmon node" "found at $HWMON"
else
	record fail hwmon "hwmon node" "no hwmon directory under $HP_WMI_DIR/hwmon"
fi

check_fan() {
	id="$1"
	title="$2"
	path="${HWMON:+$HWMON/${id}_input}"

	if [ -z "$path" ] || [ ! -e "$path" ]; then
		record skip "$id" "$title" "not exposed by this driver"
		return
	fi
	raw="$(read_value "$path")" || {
		record fail "$id" "$title" "$path could not be read"
		return
	}
	if ! is_number "$raw"; then
		record fail "$id" "$title" "$path contains something that is not a number"
		return
	fi
	# hp-wmi encodes fan-cleaner reverse spin in the value itself.
	if [ "$raw" -ge 12800 ]; then
		actual=$(((raw / 100 & 127) * 100))
		record warn "$id" "$title" \
			"$raw raw -> $actual rpm, spinning in reverse (fan cleaner active)"
	elif [ "$raw" -gt 25000 ]; then
		record warn "$id" "$title" "$raw rpm is implausibly high"
	else
		record pass "$id" "$title" "$raw rpm"
	fi
}

check_fan fan1 "Fan 1 speed"
check_fan fan2 "Fan 2 speed"

PWM="${HWMON:+$HWMON/pwm1}"
PWM_ENABLE="${HWMON:+$HWMON/pwm1_enable}"

if [ -z "$PWM" ] || [ ! -e "$PWM" ]; then
	record fail pwm1 "PWM channel" "pwm1 is not exposed, so fan speed cannot be set" \
		"Needs a kernel whose hp-wmi supports this board (see the notice below)."
else
	pwm_value="$(read_value "$PWM")" || pwm_value=""
	if ! is_number "$pwm_value"; then
		record fail pwm1 "PWM channel" "pwm1 is not a number"
	elif [ "$pwm_value" -le 255 ]; then
		record pass pwm1 "PWM channel" "pwm1 = $pwm_value (0-255)"
	else
		record warn pwm1 "PWM channel" "pwm1 = $pwm_value, outside the documented 0-255 range"
	fi
fi

if [ -z "$PWM_ENABLE" ] || [ ! -e "$PWM_ENABLE" ]; then
	record fail pwm1_enable "Fan control mode" "pwm1_enable is not exposed"
else
	mode="$(read_value "$PWM_ENABLE")" || mode=""
	case "$mode" in
	0) record pass pwm1_enable "Fan control mode" "0 - max (firmware overridden to full speed)" ;;
	1) record pass pwm1_enable "Fan control mode" "1 - manual (pwm1 is in effect)" ;;
	2) record pass pwm1_enable "Fan control mode" "2 - automatic (firmware curve)" ;;
	*) record warn pwm1_enable "Fan control mode" "$mode - unknown mode" ;;
	esac
fi

# The only check that writes. It writes the value that is already set, so
# no fan changes speed, and restores the previous mode even on failure.
if [ -z "$PWM" ] || [ ! -e "$PWM" ] || [ ! -e "$PWM_ENABLE" ]; then
	record skip pwm-write "PWM accepts writes" "no PWM channel to write to"
elif [ "$ALLOW_WRITES" -eq 0 ]; then
	record skip pwm-write "PWM accepts writes" "not attempted; enable writes to test this"
elif [ ! -w "$PWM" ] || [ ! -w "$PWM_ENABLE" ]; then
	record skip pwm-write "PWM accepts writes" "permission denied; run as root to test writes"
else
	original_mode="$(read_value "$PWM_ENABLE")"
	original_pwm="$(read_value "$PWM")"
	write_ok=1
	printf '1' >"$PWM_ENABLE" 2>/dev/null || write_ok=0
	[ "$write_ok" -eq 1 ] && { printf '%s' "$original_pwm" >"$PWM" 2>/dev/null || write_ok=0; }
	readback="$(read_value "$PWM" 2>/dev/null || echo '')"

	# Restore before interpreting anything.
	printf '%s' "$original_pwm" >"$PWM" 2>/dev/null
	restored=1
	printf '%s' "$original_mode" >"$PWM_ENABLE" 2>/dev/null || restored=0

	if [ "$write_ok" -eq 0 ]; then
		record fail pwm-write "PWM accepts writes" "the driver rejected the write"
	elif [ "$readback" = "$original_pwm" ]; then
		if [ "$restored" -eq 1 ]; then
			record pass pwm-write "PWM accepts writes" \
				"wrote and read back pwm1 = $original_pwm without changing fan speed"
		else
			record warn pwm-write "PWM accepts writes" \
				"write worked, but the original fan mode could not be restored; set it manually or reboot"
		fi
	else
		record warn pwm-write "PWM accepts writes" \
			"wrote $original_pwm but read back $readback; the driver may quantise or ignore values"
	fi
fi

# What the hwmon node actually exposes. Without this a missing pwm1 is a
# dead end: the report says the file isn't there but not what is, which is
# the first thing anyone diagnosing a partially-supported board needs.
if [ -z "$HWMON" ]; then
	record skip hwmon-attrs "hwmon attributes" "no hwmon node"
else
	attrs=""
	for entry in "$HWMON"/*; do
		[ -e "$entry" ] || continue
		base="${entry##*/}"
		case "$base" in
		device | subsystem | power | uevent) continue ;;
		esac
		attrs="${attrs:+$attrs }$base"
	done
	if [ -z "$attrs" ]; then
		record warn hwmon-attrs "hwmon attributes" "the hwmon node is empty"
	else
		record pass hwmon-attrs "hwmon attributes" "$attrs"
	fi
fi

# hp-wmi's own kernel messages usually say why a board came up with
# reduced functionality.
# Via `dmesg` rather than /dev/kmsg: reading that device directly can block
# waiting for new messages, and it is root-only wherever
# kernel.dmesg_restrict is set.
if ! command -v dmesg >/dev/null 2>&1; then
	record skip kernel-log "hp-wmi kernel messages" "dmesg is not available"
elif ! klog="$(dmesg 2>/dev/null)"; then
	record skip kernel-log "hp-wmi kernel messages" \
		"kernel log not readable; run as root, or paste \`dmesg | grep -i hp.wmi\`"
else
	hp_lines="$(printf '%s\n' "$klog" | grep -i -e 'hp-wmi' -e 'hp_wmi' | tail -4 |
		sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | tr '\n' '|' | sed 's/|$//;s/|/ | /g')"
	if [ -z "$hp_lines" ]; then
		record pass kernel-log "hp-wmi kernel messages" "no hp-wmi messages"
	else
		record warn kernel-log "hp-wmi kernel messages" "$hp_lines"
	fi
fi

if [ -r /sys/firmware/acpi/platform_profile ] && [ -r /sys/firmware/acpi/platform_profile_choices ]; then
	record pass platform-profile "ACPI platform profile" \
		"$(read_value /sys/firmware/acpi/platform_profile) (available: $(read_value /sys/firmware/acpi/platform_profile_choices))"
else
	record warn platform-profile "ACPI platform profile" \
		"not exposed; power modes fall back to power-profiles-daemon or the CPU EPP hint"
fi

if [ -z "$CPU_TEMP" ]; then
	record skip cpu-temp "CPU temperature" "not exposed by this driver"
else
	millidegrees="$(read_value "$CPU_TEMP")" || millidegrees=""
	if is_number "$millidegrees"; then
		celsius=$((millidegrees / 1000))
		if [ "$celsius" -ge 0 ] && [ "$celsius" -le 125 ]; then
			record pass cpu-temp "CPU temperature" "$celsius °C"
		else
			record warn cpu-temp "CPU temperature" "$celsius °C is implausible"
		fi
	else
		record fail cpu-temp "CPU temperature" "$CPU_TEMP contains something that is not a number"
	fi
fi

if [ -e /proc/acpi/call ]; then
	record pass acpi-call "acpi_call module (fan cleaner)" "/proc/acpi/call is available"
else
	record warn acpi-call "acpi_call module (fan cleaner)" \
		"/proc/acpi/call not found; only the dust-removal fan cleaner needs it" \
		"Install acpi_call-dkms (Arch), acpi-call-dkms (Debian) or akmod-acpi_call (Fedora), then modprobe acpi_call."
fi

# --- verdict -----------------------------------------------------------

# Derived from the check results, never from whether a path exists: an HP
# laptop can have an hwmon node with no pwm1 behind it, which is exactly the
# machine this tool exists for.
fan1_status="$(status_of fan1)"
fan2_status="$(status_of fan2)"
CAN_READ=0
case "$fan1_status$fan2_status" in *pass* | *warn*) CAN_READ=1 ;; esac

CAN_WRITE=0
if [ "$(status_of pwm1)" = "pass" ] && [ "$(status_of pwm1_enable)" = "pass" ] &&
	[ "$(status_of pwm-write)" != "fail" ]; then
	CAN_WRITE=1
fi

WRITE_TESTED=0
[ "$(status_of pwm-write)" = "pass" ] && WRITE_TESTED=1

if [ "$CAN_WRITE" -eq 1 ]; then
	VERDICT="fullControl"
	VERDICT_TEXT="Fan control works"
	if [ "$WRITE_TESTED" -eq 1 ]; then
		SUMMARY="Fan control works: speeds can be read and the PWM channel accepted a write."
	else
		SUMMARY="Fan control looks available: the PWM channel is present. Re-run with writes enabled to confirm the hardware accepts them."
	fi
	NOTICE=""
	EXIT_CODE=0
elif [ "$CAN_READ" -eq 1 ]; then
	VERDICT="monitoringOnly"
	VERDICT_TEXT="Monitoring only"
	SUMMARY="Fan speeds can be read, but this driver exposes no PWM channel, so speed cannot be set."
	if [ "$HAS_HP_WMI" -eq 1 ]; then
		NOTICE="The kernel's hp-wmi has no pwm1 for this board. Recent kernels ship manual fan control upstream, so upgrading the kernel is the first thing to try. Failing that, a patched out-of-tree driver exists: https://github.com/arfelious/omen-fan-control. Installing it replaces a kernel module, so it is a deliberate step."
	else
		NOTICE=""
	fi
	EXIT_CODE=1
else
	VERDICT="unsupported"
	VERDICT_TEXT="No fan-control interface"
	if [ -n "$CPU_TEMP" ]; then
		SUMMARY="This machine exposes no HP fan-control interface. Temperature can still be read, so monitoring works."
	else
		SUMMARY="This machine exposes no HP fan-control interface. No usable temperature sensor was found either."
	fi
	if [ "$HAS_HP_WMI" -eq 0 ]; then
		NOTICE="No hp-wmi device at all. On an HP OMEN or Victus laptop, load the hp_wmi module; on other hardware this is expected and fan control is not applicable."
	else
		NOTICE=""
	fi
	EXIT_CODE=2
fi

# --- output ------------------------------------------------------------

json_escape() {
	printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

if [ "$AS_JSON" -eq 1 ]; then
	# Same top-level shape as `pyren-check --json`, so either can be
	# pasted into a bug report and read the same way.
	printf '{\n  "system": {"vendor": "%s", "model": "%s", "boardName": "%s", "kernel": "%s"},\n' \
		"$(json_escape "$(read_value /sys/class/dmi/id/sys_vendor 2>/dev/null || echo '')")" \
		"$(json_escape "$(read_value /sys/class/dmi/id/product_name 2>/dev/null || echo '')")" \
		"$(json_escape "$(read_value /sys/class/dmi/id/board_name 2>/dev/null || echo '')")" \
		"$(json_escape "$(uname -r)")"
	printf '  "fan": {\n    "verdict": "%s",\n    "summary": "%s",\n' "$VERDICT" "$(json_escape "$SUMMARY")"
	if [ -n "$NOTICE" ]; then
		printf '    "driverNotice": "%s",\n' "$(json_escape "$NOTICE")"
	else
		printf '    "driverNotice": null,\n'
	fi
	printf '    "wroteToHardware": %s,\n    "checks": [\n' \
		"$([ "$WRITE_TESTED" -eq 1 ] && echo true || echo false)"
	first=1
	while IFS="$(printf '\t')" read -r status id title detail remedy; do
		[ "$first" -eq 1 ] || printf ',\n'
		first=0
		printf '      {"id": "%s", "title": "%s", "status": "%s", "detail": "%s", "remedy": ' \
			"$id" "$(json_escape "$title")" "$status" "$(json_escape "$detail")"
		if [ -n "$remedy" ]; then
			printf '"%s"}' "$(json_escape "$remedy")"
		else
			printf 'null}'
		fi
	done <"$CHECKS"
	printf '\n    ]\n  }\n}\n'
	exit "$EXIT_CODE"
fi

echo "pyren-check"
echo
echo "  machine  $(read_value /sys/class/dmi/id/sys_vendor 2>/dev/null || echo unknown) $(read_value /sys/class/dmi/id/product_name 2>/dev/null || echo '') (board $(read_value /sys/class/dmi/id/board_name 2>/dev/null || echo '?'))"
echo "  kernel   $(uname -r)"
echo

passed=0
failed=0
while IFS="$(printf '\t')" read -r status id title detail remedy; do
	case "$status" in
	pass)
		marker="[ ok ]"
		passed=$((passed + 1))
		;;
	fail)
		marker="[FAIL]"
		failed=$((failed + 1))
		;;
	warn) marker="[warn]" ;;
	*) marker="[skip]" ;;
	esac
	printf '  %s  %-28s %s\n' "$marker" "$title" "$detail"
	[ -n "$remedy" ] && printf '        %s\n' "$remedy" | fold -s -w 68 | sed '2,$s/^/        /'
done <"$CHECKS"

echo
echo "  $passed passed, $failed failed"
echo
echo "  $VERDICT_TEXT"
printf '%s\n' "$SUMMARY" | fold -s -w 72 | sed 's/^/  /'
if [ -n "$NOTICE" ]; then
	echo
	printf '%s\n' "$NOTICE" | fold -s -w 72 | sed 's/^/  ! /'
fi

exit "$EXIT_CODE"
