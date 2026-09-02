# Findings

Things that took real work to establish. Written down so nobody has to
re-derive them, and so that a surprising piece of code has a reason
attached.

## Board 8D2F: why fan control doesn't work on the test laptop

The one HP machine this has run on is an **OMEN Gaming Laptop 16-am0xxx,
board `8D2F`, kernel 7.2.2**. `omen-hub-check` reports `monitoringOnly`:

| | |
|---|---|
| `hp-wmi` loaded | yes |
| hwmon node | yes (`hwmon8`) |
| `fan1_input` / `fan2_input` | readable (0 rpm at idle) |
| **`pwm1`** | **absent** |
| `pwm1_enable` | present, reads `2` (firmware curve) |
| `platform_profile` | absent |

`pwm1_enable` without `pwm1` is unusual — hwmon normally exposes them
together — and `platform_profile` being absent on an OMEN is also odd. Both
suggest the driver came up with reduced functionality for this board.

Reading the patched driver's source (`hp_wmi_hwmon_is_visible`) shows
`hwmon_pwm` returning `0644` unconditionally, so that driver *would* expose
`pwm1`. But the deciding factor is elsewhere:
`hp_wmi_fan_control_supported()` returns `params && params->get_fan_speed`,
where `params` comes from `active_board_params`, which is set from the
`hp_wmi_feature_boards` DMI table.

**`8D2F` appears in `omen_thermal_profile_boards` but not in
`hp_wmi_feature_boards`.** So installing the patched driver as-is would
probably *still* not give working PWM control — the board would need an
entry in the feature table with the right params variant
(`victus_s`, `omen_v1`, `omen_v1_legacy`, `omen_v1_no_ec`). That is exactly
what the installer's `experimentalBoard` + `boardTable` parameters do, and
why `boardTable` is required rather than guessed: the variants differ in
which EC offset holds the thermal profile, so the wrong one gives a driver
that loads and then misreads the hardware.

**Still unverified**, because this reasons about the *patched* driver while
the laptop runs the *stock* one. Two data points would settle it, and the
current `omen-check.sh` collects both:

- the full listing of `/sys/devices/platform/hp-wmi/hwmon/hwmon8/`
- `dmesg | grep -i hp.wmi` (needs root; `kernel.dmesg_restrict` is set)

Run `sudo sh omen-check.sh --json`. The result on file as of writing is
from the older version and has neither.

## Manual fan control is upstream now

Recent kernels ship it (the source project's README says ≥ 6.20; the
project owner says 7.0 — either way both test machines are on 7.2). This is
why the project pivoted from *installing* a patched driver to *verifying*
what the running kernel already does.

Detection does not rely on the version number: `fan.diagnose` probes for
`pwm1`, which is the actual question. The version only feeds an advisory
message.

## The source project's docs disagree with its own driver

Found by testing the installer's patcher against the real `hp-wmi.c` rather
than only a fixture. The code is what gets compiled, so the port follows
the code and keeps the documented names as fallbacks.

| documented | actually in `hp-wmi.c` |
|---|---|
| `#define OMEN_MAX_RPM` | `OMEN_CPU_MAX_RPM` **and** `OMEN_GPU_MAX_RPM` |
| `victus_s_thermal_profile_boards[]` | no such symbol; the real table is `hp_wmi_feature_boards[]`, a `dmi_system_id` whose entries also select a board-params variant |

Patching by the documented names silently does nothing, which for the
max-RPM constant means an uncalibrated fan ceiling.

## The driver source is not vendored, on purpose

`hp-wmi.c` is a modified copy of a GPL-2 kernel driver maintained in
[`omen-fan-control`](https://github.com/arfelious/omen-fan-control).
Copying it here would mean tracking their changes by hand.

Researched alternatives, should this ever need revisiting:

- **No distro package exists.** The AUR has nothing for `hp-wmi-omen` or
  `omen-fan-control` (checked directly against the AUR RPC). The project
  ships PKGBUILDs users build themselves.
- Upstream is active (GPL-3.0, tagged releases), so **fetching a pinned
  release** is viable and is the only option that removes the maintenance
  entirely — the burden is the *patch*, not the copy, and fetching leaves
  that burden upstream.
- If that is ever built: pin the **sha256 of each extracted file**, not of
  the archive. GitHub's auto-generated tarballs have changed compression
  before and broken archive hashes across the ecosystem. And pin something:
  this is code compiled into the kernel, so an unpinned download is remote
  code execution in ring 0.
- The copy currently on the developer's disk is **upstream `main`, not any
  release** (`2eab8333…`, ≠ `v2.0.0`'s `f2c92786…`) — nobody knows which
  version it is, which is the problem pinning solves.

## The RGB project has two unrelated hardware paths

Full review in [`docs/04-rgb-porting-review.md`](../docs/04-rgb-porting-review.md).
Short version: per-key RGB over USB HID (`0d62:54bf`) and a 4-zone lightbar
over ACPI share nothing, and which applies is not decided by the model
name. Nothing can be ported until `lsusb | grep -i 0d62` is run on the
laptop.

That review also found: a contradiction between `set_all()` and
`data/keys.json` about backspace, an `lstrip("b0x")` that eats data bytes
(it takes a character set, not a prefix — verified), and that
`/proc/acpi/call` is a single global interface with no locking, which a
daemon must serialise **across modules** since the fan cleaner uses it too.

## Bugs the parity test caught

`tools/omen-check.sh` and `omen-hub-check` are compared against shared
fixtures by `daemon/check/tests/parity.rs`. It has earned its place three
times, and every time the Rust side was the wrong one:

1. **The verdict came from path presence, not check results.** Discovery
   fills in every sysfs path as soon as an hwmon node exists, whether the
   files behind them do or not — so a machine with an hwmon node and no
   `pwm1` reported "fan control available" and exit status 0. On exactly
   the machines this tool exists for. The unit test had masked it by
   setting the path to `None` by hand, which real discovery never does.
2. **A missing `fan2_input` was a failure.** A one-fan machine is not a
   broken machine.
3. **Reading `/dev/kmsg` directly can block** waiting for new messages, and
   is root-only under `kernel.dmesg_restrict`. Both versions now shell out
   to `dmesg`.

The lesson worth keeping: **derive state from what was observed, not from
what was constructed.** All three are the same mistake wearing different
hats.

## Peripherals register as batteries

`/sys/class/power_supply` includes wireless mice and keyboards. Without
filtering on `scope=Device`, a discharging Logitech mouse makes a desktop
look like an unplugged laptop, and the power supervisor drops it to Eco.
Found by running the code on a desktop.

## Arch has an `update-initramfs` shim

The shell installer this was ported from picks `update-initramfs` first
unconditionally. On Arch-family systems a compatibility shim by that name
is often installed next to the real `mkinitcpio` — as on the development
machine. The port picks the generator matching the distribution family
first.
