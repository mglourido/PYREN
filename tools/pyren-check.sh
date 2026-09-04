#!/bin/sh
# pyren-check.sh - what this machine can actually be told to do.
#
# Portable stand-in for `pyren-check` (daemon/check), for when building
# the project isn't practical: copy this one file to the laptop and run it.
# POSIX sh, no dependencies beyond coreutils.
#
# Three surfaces, one verdict: fans, power modes and lighting. It performs
# the same checks, in the same order, with the same statuses, verdict and
# exit code as the Rust version. `daemon/check/tests/parity.rs` compares
# the two against fixtures so they cannot drift apart silently.
#
#   ./pyren-check.sh            read-only, safe on any machine
#   sudo ./pyren-check.sh -w    also verify the PWM accepts writes
#   ./pyren-check.sh --json     machine-readable, for bug reports
#
# Exit status is about *fans*, because that is what scripts branch on:
# 0 full control, 1 monitoring only, 2 no interface. The compatibility
# verdict is wider - a machine with no fan control can still have power
# modes and a lightbar - so read the last line, not $?.

set -u

HP_WMI_DIR="/sys/devices/platform/hp-wmi"
CPU_ROOT="/sys/devices/system/cpu"
POWERCAP="/sys/class/powercap"
# PYREN_ACPI_CALL and PYREN_USB_DEVICES point the lighting checks at
# fixtures, the way PYREN_HWMON_DIR does for the fan ones. Same names as
# the Rust version, so the parity test can drive both.
ACPI_CALL="${PYREN_ACPI_CALL:-/proc/acpi/call}"
USB_DEVICES="${PYREN_USB_DEVICES:-/sys/bus/usb/devices}"
ALLOW_WRITES=0
AS_JSON=0

usage() {
	cat <<'USAGE'
pyren-check.sh - what this machine can actually be told to do

USAGE:
    pyren-check.sh [OPTIONS]

Checks three surfaces - fans, power modes and lighting - and prints one
verdict: the same one the daemon prints at startup.

OPTIONS:
    -w, --write   Also verify that the PWM channel accepts writes. Rewrites
                  the value already set and restores the previous mode, so
                  no fan changes speed. Needs root.
    -j, --json    Print the report as JSON.
    -h, --help    Show this help.

EXIT STATUS is about fan control, which is what scripts branch on:
    0  fan control works
    1  fan speeds can be read but not set
    2  no HP fan-control interface on this machine

The overall verdict is wider than that, so read the last line for the
compatibility answer.
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

# record <status> <id> <title> <detail> [remedy], filed under $SECTION.
# Fields are tab-separated, so details may contain spaces. The section is a
# variable rather than an argument so that adding two more sections did not
# mean editing every existing call site.
SECTION="fan"
record() {
	printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$SECTION" "$1" "$2" "$3" "$4" "${5:-}" >>"$CHECKS"
}

# Check ids are unique across sections, so a lookup needs no section.
status_of() {
	awk -F'\t' -v id="$1" '$3 == id { print $2; exit }' "$CHECKS"
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

# Microwatts as whole watts, the unit a person reads a power limit in.
microwatts() {
	if is_number "$1"; then
		echo "$(($1 / 1000000)) W"
	else
		echo "-"
	fi
}

# Whether an acpi_call reply is the firmware's "PASS". Mirrors
# pyren_rgb::lightbar::is_success: the four bytes may come back as one hex
# blob, as a {0x50, 0x41, ...} list, or as the letters themselves.
is_acpi_pass() {
	[ -n "$1" ] || return 1
	upper="$(printf '%s' "$1" | tr 'a-f' 'A-F')"
	case "$upper" in
	*50415353* | *PASS*) return 0 ;;
	esac
	# {0x50, 0x41, 0x53, 0x53} -> 50415353
	packed="$(printf '%s' "$upper" | tr -d '{}, \t' | sed 's/0X//g')"
	case "$packed" in
	50415353*) return 0 ;;
	esac
	return 1
}

# An acpi_call reply as one lowercase hex string. Mirrors
# pyren_core::acpi::parse_bytes: a {0x50, 0x41, ...} list and a bare blob
# both collapse to the same digits, and a leading "0x" or "b" is dropped
# once rather than character by character.
acpi_hex() {
	printf '%s' "$1" |
		tr -d '\000{}, \t\n' |
		tr 'A-F' 'a-f' |
		sed -e 's/0x//g' -e 's/^b//'
}

# Byte N (zero-based) of such a string, as a decimal number. Empty when the
# reply is too short to have one, which every caller checks for.
hex_byte() {
	byte="$(printf '%s' "$1" | cut -c "$((${2} * 2 + 1))-$((${2} * 2 + 2))")"
	[ "${#byte}" -eq 2 ] || return 0
	printf '%d' "0x${byte}" 2>/dev/null || true
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
	hp_all="$(printf '%s\n' "$klog" | grep -i -e 'hp-wmi' -e 'hp_wmi')"
	hp_lines="$(printf '%s\n' "$hp_all" | tail -4 |
		sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | tr '\n' '|' | sed 's/|$//;s/|/ | /g')"
	# Mirrors is_concerning()/is_unsigned_module_notice() in
	# crates/fan/src/diagnostics.rs, and the parity test compares the status
	# these produce - so the two lists have to stay identical.
	#
	# The taint notice is dropped before the search for trouble because it
	# contains the word "failed": loading a module the distribution did not
	# sign is what the driver installer does, so it is expected, and matching
	# on it gave a working machine a yellow row with nothing wrong in it.
	hp_bad="$(printf '%s\n' "$hp_all" |
		grep -iv -e 'module verification failed' -e 'tainting kernel' |
		grep -iE 'fail|error|unknown ec layout|cannot|unable|not supported|reduced')"
	if [ -z "$hp_all" ]; then
		record pass kernel-log "hp-wmi kernel messages" "no hp-wmi messages"
	elif [ -n "$hp_bad" ]; then
		record warn kernel-log "hp-wmi kernel messages" "$hp_lines"
	elif printf '%s\n' "$hp_all" | grep -qi -e 'module verification failed' -e 'tainting kernel'; then
		record pass kernel-log "hp-wmi kernel messages" \
			"Nothing wrong here. The 'module verification failed' line is the kernel noting that a module built outside your distribution's kernel package was loaded, and tainting itself to record that - which is exactly what installing the patched hp-wmi does, so it is expected rather than a fault: $hp_lines"
	else
		record pass kernel-log "hp-wmi kernel messages" "$hp_lines"
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

# Two things need this now, not one: the RGB lightbar and the dust-removal
# fan cleaner. Naming only the cleaner made the warning look optional to
# anyone who does not want it.
if [ -e "$ACPI_CALL" ]; then
	record pass acpi-call "acpi_call module" "/proc/acpi/call is available"
else
	record warn acpi-call "acpi_call module" \
		"/proc/acpi/call not found; the RGB lightbar and the fan cleaner both need it" \
		"Install acpi_call-dkms (Arch), acpi-call-dkms (Debian) or akmod-acpi_call (Fedora), then modprobe acpi_call."
fi

# The dust-removal fan cleaner. Two capability *queries* - command type 44
# asks, 46 sets - so this commands nothing; it is the same class of
# question the lightbar read below puts. Kept in step with
# pyren_fan::cleaner::probe by daemon/check/tests/parity.rs.
#
# ("SECU", command 0x20008, type 44, size 128) + 128 zero bytes, and the
# 4-byte legacy buffer ("SECU", command 1, type 44, size 4) + 4 zeros.
CLEANER_MODERN_GET="b53454355080002002c00000080000000$(printf '%0256d' 0)"
CLEANER_LEGACY_GET="b53454355010000002c00000004000000$(printf '%08d' 0)"

# 1 the firmware answered yes, 0 it was asked and said no, "" never asked.
CLEANER=""
CLEANER_DETAIL=""
if [ -e "$ACPI_CALL" ]; then
	CLEANER_UNREACHABLE=0
	CLEANER=0

	# Modern ("CleanCreek"): byte 8 of the data past the 8-byte reply
	# header is the capability bitmask - bit 0 the CPU fan, bit 1 the GPU
	# fan, bit 2 a third. So byte 16 of the whole reply.
	if reply="$(
		{
			printf '%s' "\\_SB.WMID.WMAA 0 3 $CLEANER_MODERN_GET" >"$ACPI_CALL" &&
				tr -d '\000' <"$ACPI_CALL"
		} 2>/dev/null
	)"; then
		hex="$(acpi_hex "$reply")"
		code="$(hex_byte "$hex" 4)"
		mask="$(hex_byte "$hex" 16)"
		if [ -n "$mask" ] && [ "${code:-1}" -eq 0 ] && [ "$mask" -ne 0 ]; then
			CLEANER=1
			CLEANER_DETAIL="the firmware answered: reverse spin is available"
		fi
	else
		CLEANER_UNREACHABLE=1
	fi

	# Legacy: bit 5 of the first data byte, i.e. byte 8 of the reply.
	if [ "$CLEANER" -eq 0 ] && [ "$CLEANER_UNREACHABLE" -eq 0 ]; then
		if reply="$(
			{
				printf '%s' "\\_SB.WMID.WMAA 0 2 $CLEANER_LEGACY_GET" >"$ACPI_CALL" &&
					tr -d '\000' <"$ACPI_CALL"
			} 2>/dev/null
		)"; then
			hex="$(acpi_hex "$reply")"
			code="$(hex_byte "$hex" 4)"
			flags="$(hex_byte "$hex" 8)"
			if [ -n "$flags" ] && [ "${code:-1}" -eq 0 ] && [ $((flags & 32)) -ne 0 ]; then
				CLEANER=1
				CLEANER_DETAIL="the firmware answered: the older single-speed fan cleaner is available"
			fi
		else
			CLEANER_UNREACHABLE=1
		fi
	fi

	if [ "$CLEANER_UNREACHABLE" -eq 1 ]; then
		CLEANER=""
	fi
fi

if [ "$CLEANER" = "1" ]; then
	record pass fan-cleaner "Fan cleaner (reverse spin)" "$CLEANER_DETAIL"
elif [ "$CLEANER" = "0" ]; then
	record warn fan-cleaner "Fan cleaner (reverse spin)" \
		"the firmware was asked and has no fan cleaner on this machine"
elif [ -e "$ACPI_CALL" ]; then
	record skip fan-cleaner "Fan cleaner (reverse spin)" \
		"writing /proc/acpi/call needs root" \
		"The firmware is asked over /proc/acpi/call, which needs the acpi_call module loaded and root to write. With both, run this again."
elif command -v modinfo >/dev/null 2>&1 && modinfo -n acpi_call >/dev/null 2>&1; then
	record skip fan-cleaner "Fan cleaner (reverse spin)" \
		"acpi_call is installed but not loaded, so the firmware was not asked" \
		"The firmware is asked over /proc/acpi/call, which needs the acpi_call module loaded and root to write. With both, run this again."
else
	record skip fan-cleaner "Fan cleaner (reverse spin)" \
		"/proc/acpi/call is missing, so the firmware was not asked" \
		"The firmware is asked over /proc/acpi/call, which needs the acpi_call module loaded and root to write. With both, run this again."
fi


# --- power -------------------------------------------------------------

SECTION="power"

# Mechanisms, in the same order as pyren_power::backend::read_state, since
# the report lists them in the order they were found.
MECHANISMS=""
add_mechanism() { MECHANISMS="${MECHANISMS:+$MECHANISMS, }$1"; }

PLATFORM_PROFILE=""
PLATFORM_PROFILE_CHOICES=""
if [ -r /sys/firmware/acpi/platform_profile ]; then
	PLATFORM_PROFILE="$(read_value /sys/firmware/acpi/platform_profile)"
	add_mechanism platform_profile
fi
if [ -r /sys/firmware/acpi/platform_profile_choices ]; then
	PLATFORM_PROFILE_CHOICES="$(read_value /sys/firmware/acpi/platform_profile_choices)"
fi
# Asking powerprofilesctl rather than looking for a unit file: what matters
# is whether it answers, which is also what the daemon asks.
if command -v powerprofilesctl >/dev/null 2>&1 &&
	ppd="$(powerprofilesctl get 2>/dev/null)" && [ -n "$ppd" ]; then
	add_mechanism power-profiles-daemon
fi
if [ -r "$CPU_ROOT/cpu0/cpufreq/energy_performance_preference" ]; then
	add_mechanism energy_performance_preference
fi

if [ -z "$MECHANISMS" ]; then
	record warn power-mechanisms "Power-mode mechanisms" \
		"none - no ACPI platform profile, no power-profiles-daemon, no EPP hint" \
		"This is normal on a desktop. On a laptop, power-profiles-daemon is the usual provider: install and enable it (systemctl enable --now power-profiles-daemon)."
elif [ -n "$PLATFORM_PROFILE" ] && [ -n "$PLATFORM_PROFILE_CHOICES" ]; then
	record pass power-mechanisms "Power-mode mechanisms" \
		"$MECHANISMS (platform profile $PLATFORM_PROFILE, choices: $(printf '%s' "$PLATFORM_PROFILE_CHOICES" | tr ' ' ',' | sed 's/,/, /g'))"
else
	record pass power-mechanisms "Power-mode mechanisms" "$MECHANISMS"
fi

# The package RAPL zone. The mmio interface addresses the same package, so
# one is enough; the lowest-numbered package zone wins, as in the daemon.
RAPL_ZONE=""
for zone in "$POWERCAP"/intel-rapl:*; do
	[ -d "$zone" ] || continue
	case "${zone##*/}" in *mmio*) continue ;; esac
	[ -r "$zone/name" ] || continue
	case "$(read_value "$zone/name")" in
	package-*)
		RAPL_ZONE="$zone"
		break
		;;
	esac
done

# Constraints 0/1/2 are PL1/PL2/PL4, as in pyren_power::limits::read. PL4
# is not printed - nothing here sets it - but it counts towards "is there
# an envelope at all", or a machine exposing only PL4 would disagree with
# the Rust version about whether it has one.
PL1_UW=""
PL2_UW=""
PL4_UW=""
if [ -n "$RAPL_ZONE" ]; then
	PL1_UW="$(read_value "$RAPL_ZONE/constraint_0_power_limit_uw" 2>/dev/null || echo '')"
	PL2_UW="$(read_value "$RAPL_ZONE/constraint_1_power_limit_uw" 2>/dev/null || echo '')"
	PL4_UW="$(read_value "$RAPL_ZONE/constraint_2_power_limit_uw" 2>/dev/null || echo '')"
fi
if [ -z "$PL1_UW" ] && [ -z "$PL2_UW" ] && [ -z "$PL4_UW" ]; then
	record warn power-envelope "Package power envelope" \
		"no RAPL package zone, so PL1/PL2 cannot be read or set"
else
	record pass power-envelope "Package power envelope" \
		"PL1 $(microwatts "$PL1_UW"), PL2 $(microwatts "$PL2_UW")"
fi

if [ -e "$CPU_ROOT/intel_pstate/no_turbo" ] || [ -e "$CPU_ROOT/cpufreq/boost" ]; then
	record pass power-turbo "Turbo / boost switch" \
		"exposed, so turbo can be switched per mode"
	HAS_TURBO=1
else
	record warn power-turbo "Turbo / boost switch" "not exposed; modes leave turbo alone"
	HAS_TURBO=0
fi

if [ -z "$MECHANISMS" ]; then
	POWER_SUMMARY="No power-mode mechanism answered, so the modes would have nothing to drive. The envelope, if there is one, can still be set directly."
else
	POWER_SUMMARY="Power modes are available through $MECHANISMS."
fi

# --- lighting ----------------------------------------------------------

SECTION="lighting"

# Both paths are reported whatever this machine has: per-key RGB over USB
# HID and a 4-zone lightbar over ACPI share nothing, and which one a laptop
# has is not decided by its model name - so one "no lighting" line would
# answer the question for neither.

PER_KEY_ID="0d62:54bf"
PER_KEY=0
for device in "$USB_DEVICES"/*; do
	[ -r "$device/idVendor" ] && [ -r "$device/idProduct" ] || continue
	[ "$(read_value "$device/idVendor")" = "0d62" ] || continue
	[ "$(read_value "$device/idProduct")" = "54bf" ] || continue
	PER_KEY=1
	break
done

if [ "$PER_KEY" -eq 1 ]; then
	record warn lighting-per-key "Per-key RGB keyboard" \
		"$PER_KEY_ID is attached, but this build does not drive it" \
		"The per-key path is deliberately unported until the key map's backspace entry can be checked on real hardware; see docs/04-rgb-porting-review.md."
else
	record skip lighting-per-key "Per-key RGB keyboard" "no $PER_KEY_ID on this machine"
fi

# There is no single OMEN lighting protocol: three unrelated ways of
# talking to these lights exist, and which one a laptop speaks is not
# decided by its model name. All three are probed with a *read*, in the
# order the daemon tries them, and each gets its own check - because "no
# lighting" is three different findings with three different next steps.
#
# The two WMI buffers below are byte for byte what the daemon sends, and
# daemon/check/tests/parity.rs is what keeps them that way.
#   fourZone: header ("SECU", command 0x20009, type 2, size 128) + 128 zeros
#   lightbar: header ("SECU", command 0x20008, type 4, size 128) + 128 zeros
FOURZONE_GET="b53454355090002000200000080000000$(printf '%0256d' 0)"
LIGHTBAR_GET="b53454355080002000400000080000000$(printf '%0256d' 0)"

RGB_ZONES_DIR="${PYREN_RGB_ZONES_DIR:-/sys/devices/platform/hp-wmi/rgb_zones}"

ACPI_CALL_INSTALLED=0
if [ -e "$ACPI_CALL" ]; then
	ACPI_CALL_INSTALLED=1
elif command -v modinfo >/dev/null 2>&1 && modinfo -n acpi_call >/dev/null 2>&1; then
	ACPI_CALL_INSTALLED=1
fi

# Why a WMI dialect could not even be asked, or "" when it could. Shared by
# both of them, because they need the same two things.
wmi_skip_reason() {
	if [ ! -d "$HP_WMI_DIR" ]; then
		echo "no hp-wmi interface on this machine"
	elif [ ! -e "$ACPI_CALL" ]; then
		echo "/proc/acpi/call is not there, so the firmware cannot be asked"
	fi
}

DRIVEN=""

# The kernel's own files, first: it is the only dialect that cannot send
# the firmware a command it did not expect, so wherever it exists it is the
# right answer.
if [ -e "$RGB_ZONES_DIR/zone00" ] && head -c 6 "$RGB_ZONES_DIR/zone00" >/dev/null 2>&1; then
	record pass lighting-kernelZones "Lighting dialect: kernelZones" \
		"answered a read of all four zones"
	DRIVEN="${DRIVEN:-kernelZones}"
else
	record skip lighting-kernelZones "Lighting dialect: kernelZones" \
		"this kernel does not publish rgb_zones for hp-wmi"
fi

# One WMI dialect: probe it with its own read and record the result.
# $1 id, $2 the buffer to send.
probe_wmi_dialect() {
	_id="$1"
	_buffer="$2"
	_skip="$(wmi_skip_reason)"
	if [ -n "$_skip" ]; then
		record skip "lighting-$_id" "Lighting dialect: $_id" "$_skip"
		return
	fi
	# stderr is redirected for the whole group, not per command: a failed
	# *redirection* is reported by the shell before the command's own
	# 2>/dev/null would apply, so a non-root run would print a raw
	# "Permission denied" over this tool's output.
	if _reply="$(
		{
			printf '%s' "\\_SB.WMID.WMAA 0 3 $_buffer" >"$ACPI_CALL" &&
				tr -d '\000' <"$ACPI_CALL"
		} 2>/dev/null
	)"; then
		if is_acpi_pass "$_reply"; then
			record pass "lighting-$_id" "Lighting dialect: $_id" \
				"answered a read of all four zones"
			DRIVEN="${DRIVEN:-$_id}"
		else
			record warn "lighting-$_id" "Lighting dialect: $_id" \
				"the firmware refused (it answered: $_reply)" \
				"This is one of several ways of talking to these lights; the others are checked separately. 'pyren-ctl rgb dialect <id>' forces one by hand."
		fi
	else
		# The interface is there and the call failed anyway - an
		# unprivileged run. Not a refusal: the fix is sudo.
		record skip "lighting-$_id" "Lighting dialect: $_id" \
			"writing /proc/acpi/call needs root, so the firmware was not asked" \
			"Writing /proc/acpi/call needs root; re-run this as root."
	fi
}

probe_wmi_dialect fourZone "$FOURZONE_GET"
probe_wmi_dialect lightbar "$LIGHTBAR_GET"

if [ -n "$DRIVEN" ]; then
	LIGHTING_SUMMARY="The lights answered on '$DRIVEN' and can be driven."
elif [ "$PER_KEY" -eq 1 ]; then
	LIGHTING_SUMMARY="A per-key RGB keyboard is attached; this build does not drive it."
else
	LIGHTING_SUMMARY="No lighting this project can drive was found. See the per-dialect checks for whether that was established or merely not asked."
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

# --- compatibility -----------------------------------------------------

# The machine gets one verdict, and this is it. Mirrors
# pyren_system::identity::classify: derived from what was observed, never
# from a board id. A second verdict per section is how a tool ends up
# disagreeing with itself in the same output.
CTRL_FAN_MODE=0
CTRL_FAN_SPEED=0
[ -n "$PWM_ENABLE" ] && [ -e "$PWM_ENABLE" ] && CTRL_FAN_MODE=1
[ -n "$PWM" ] && [ -e "$PWM" ] && CTRL_FAN_SPEED=1
CTRL_POWER_MODE=0
[ -n "$MECHANISMS" ] && CTRL_POWER_MODE=1
# Any dialect answering counts, which is what the daemon reports too. The
# wire field is still called `lightbar`; the thing it means is "the lights
# answered somewhere".
CTRL_LIGHTBAR=0
[ -n "$DRIVEN" ] && CTRL_LIGHTBAR=1

WORKS=""
add_works() { WORKS="${WORKS:+$WORKS, }$1"; }
if [ "$CTRL_FAN_SPEED" -eq 1 ]; then
	add_works "fan speed"
elif [ "$CTRL_FAN_MODE" -eq 1 ]; then
	# Worth spelling out: it is the common case on a board the driver has
	# no entry for, and "fans" alone would overpromise.
	add_works "fan mode (auto/max only)"
fi
[ "$CTRL_POWER_MODE" -eq 1 ] && add_works "power modes"
[ "$CTRL_LIGHTBAR" -eq 1 ] && add_works "lightbar colour"

if [ -n "$WORKS" ]; then
	COMPATIBILITY="controllable"
	COMPAT_REASON="this machine accepts: $WORKS"
elif [ "$HAS_HP_WMI" -eq 1 ]; then
	COMPATIBILITY="monitoringOnly"
	COMPAT_REASON="the hp-wmi interface is present but nothing here accepted control; fan speeds and temperatures can still be read"
else
	COMPATIBILITY="unsupported"
	COMPAT_REASON="no hp-wmi interface and no power-mode mechanism; monitoring works, hardware control does not"
fi

# --- output ------------------------------------------------------------

json_escape() {
	printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# Emits the checks of one section as a JSON array, indented to sit inside
# an object. One function for all three sections, so they cannot drift into
# three shapes.
json_checks() {
	first=1
	while IFS="$(printf '\t')" read -r section status id title detail remedy; do
		[ "$section" = "$1" ] || continue
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
	[ "$first" -eq 1 ] || printf '\n'
}

json_bool() {
	[ "$1" -eq 1 ] && printf 'true' || printf 'false'
}

if [ "$AS_JSON" -eq 1 ]; then
	# Same top-level shape as `pyren-check --json`, so either can be
	# pasted into a bug report and read the same way.
	printf '{\n  "system": {\n'
	printf '    "vendor": "%s", "model": "%s", "boardName": "%s", "kernel": "%s",\n' \
		"$(json_escape "$(read_value /sys/class/dmi/id/sys_vendor 2>/dev/null || echo '')")" \
		"$(json_escape "$(read_value /sys/class/dmi/id/product_name 2>/dev/null || echo '')")" \
		"$(json_escape "$(read_value /sys/class/dmi/id/board_name 2>/dev/null || echo '')")" \
		"$(json_escape "$(uname -r)")"
	printf '    "compatibility": "%s",\n    "reason": "%s",\n' \
		"$COMPATIBILITY" "$(json_escape "$COMPAT_REASON")"
	# gpuMux and networkQos are always false here: GPU MUX switching and
	# network QoS are pyren-daemon's own modules (see
	# docs/01-ipc-protocol.md), and this script - like pyren-check itself -
	# never probes them, same as it already leaves out overclock and hotkey.
	printf '    "controls": {"fanMode": %s, "fanSpeed": %s, "powerMode": %s, "lightbar": %s, "gpuMux": false, "networkQos": false}\n  },\n' \
		"$(json_bool "$CTRL_FAN_MODE")" "$(json_bool "$CTRL_FAN_SPEED")" \
		"$(json_bool "$CTRL_POWER_MODE")" "$(json_bool "$CTRL_LIGHTBAR")"

	printf '  "fan": {\n    "verdict": "%s",\n    "summary": "%s",\n' "$VERDICT" "$(json_escape "$SUMMARY")"
	if [ -n "$NOTICE" ]; then
		printf '    "driverNotice": "%s",\n' "$(json_escape "$NOTICE")"
	else
		printf '    "driverNotice": null,\n'
	fi
	printf '    "wroteToHardware": %s,\n    "checks": [\n' \
		"$([ "$WRITE_TESTED" -eq 1 ] && echo true || echo false)"
	json_checks fan
	printf '    ]\n  },\n'

	printf '  "power": {\n    "summary": "%s",\n    "checks": [\n' "$(json_escape "$POWER_SUMMARY")"
	json_checks power
	printf '    ]\n  },\n'

	printf '  "lighting": {\n    "summary": "%s",\n    "checks": [\n' "$(json_escape "$LIGHTING_SUMMARY")"
	json_checks lighting
	printf '    ]\n  }\n}\n'
	exit "$EXIT_CODE"
fi

echo "pyren-check"
echo
echo "  machine  $(read_value /sys/class/dmi/id/sys_vendor 2>/dev/null || echo unknown) $(read_value /sys/class/dmi/id/product_name 2>/dev/null || echo '') (board $(read_value /sys/class/dmi/id/board_name 2>/dev/null || echo '?'))"
echo "  kernel   $(uname -r)"

# print_section <name>; sets $passed / $failed for the caller.
print_section() {
	passed=0
	failed=0
	echo
	# "fans" reads better as a heading than the section id does.
	case "$1" in
	fan) echo "fans" ;;
	*) echo "$1" ;;
	esac
	while IFS="$(printf '\t')" read -r section status id title detail remedy; do
		[ "$section" = "$1" ] || continue
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
}

print_section fan
echo "  $passed passed, $failed failed"
printf '%s\n' "$SUMMARY" | fold -s -w 72 | sed 's/^/  /'
if [ -n "$NOTICE" ]; then
	printf '%s\n' "$NOTICE" | fold -s -w 72 | sed 's/^/  ! /'
fi

print_section power
printf '%s\n' "$POWER_SUMMARY" | fold -s -w 72 | sed 's/^/  /'

print_section lighting
printf '%s\n' "$LIGHTING_SUMMARY" | fold -s -w 72 | sed 's/^/  /'

# The one line this whole tool exists to print, so it goes last and says
# what it is rather than being another summary among four.
echo
echo "compatibility"
printf '%s\n' "$COMPAT_REASON" | fold -s -w 72 | sed 's/^/  /'

exit "$EXIT_CODE"
