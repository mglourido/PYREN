# Findings

Things that took real work to establish. Written down so nobody has to
re-derive them, and so that a surprising piece of code has a reason
attached.

## Board 8D2F: why a fan *percentage* can't be set on the test laptop

The one HP machine this has run on is an **OMEN Gaming Laptop 16-am0xxx,
board `8D2F`, kernel 7.2.2**. `pyren-check` reports `monitoringOnly`:

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
`hp_wmi_feature_boards`** (verified directly against the driver source:
line 383 versus the table at 411). The first reading of this was that a
patched driver would *still* not give working PWM control — see the next
section, which corrects that. What the board would need to be a
first-class citizen is an entry in the feature table with the right params
variant
(`victus_s`, `omen_v1`, `omen_v1_legacy`, `omen_v1_no_ec`). That is exactly
what the installer's `experimentalBoard` + `boardTable` parameters do, and
why `boardTable` is required rather than guessed: the variants differ in
which EC offset holds the thermal profile, so the wrong one gives a driver
that loads and then misreads the hardware.

**Re-confirmed on 2026-09-02**, on the laptop itself, kernel 7.2.2. The
full hwmon listing is now on file and matches the table above exactly:

```
$ ls /sys/devices/platform/hp-wmi/hwmon/hwmon*/
device  fan1_input  fan2_input  name  power  pwm1_enable  subsystem  uevent
$ cat .../pwm1_enable  →  2          # firmware curve
$ cat /sys/firmware/acpi/platform_profile  →  no such file
```

So `pwm1` is genuinely absent rather than momentarily missing, and the
`hp-wmi` platform node exposes no thermal-profile attribute either
(`als display dock hddtemp postcode tablet`, and nothing else).

This is what killed the board list. The daemon used to print *"Supported:
board 8D2F is on the known-good list"* on this machine — from a list copied
out of the driver's DMI tables — while `fan.diagnose` said `monitoringOnly`
about the same hardware, five lines later. The list was wrong in both
directions, and could only ever be fixed one board at a time for a driver
this project does not install. `compatibility` is now derived from what the
modules found they could do; see `docs/01-ipc-protocol.md`
§"`controls` and `compatibility` are measured, not looked up". The same
machine now reports:

    Controllable: this machine accepts: fan mode (auto/max only), power modes

**`dmesg` is now on file too**, and it is a single line:

```
[    5.585175] input: HP WMI hotkeys as /devices/virtual/input/input10
```

Nothing about fans, no error, no reduced-functionality warning — and
notably none of the `pr_info` lines the driver emits when it *does* find
fan hardware (`HP WMI: Hardware reported Max RPM: …`, the fan-table
messages). The driver is not failing on this board; it simply never enters
the code path, exactly as a board missing from `hp_wmi_feature_boards`
would.

## What the driver actually gates on the board table

Read from the project's own copy of the driver source
(`data/driver/hp-wmi-omen/hp-wmi.c`, which is byte-identical to its
`.orig` — the patching happens at install time, so that file is plain
upstream). This corrects the pessimistic reading above, and it is what
makes a fan write path worth building at all:

`pwm1_enable` is a *mode* switch, and the three modes do not need the same
things (`hp_wmi_apply_fan_settings`):

| value | mode | how it is applied | needs board params? |
|---|---|---|---|
| `0` | `PWM_MODE_MAX` | `hp_wmi_fan_speed_max_set(1)` | **no** |
| `1` | `PWM_MODE_MANUAL` | `hp_wmi_fan_speed_set()` → a WMI query | **no** |
| `2` | `PWM_MODE_AUTO` | `hp_wmi_fan_speed_max_set(0)` | **no** |

`hp_wmi_fan_control_supported()` — the thing `active_board_params` decides
— only affects *reading* the active fan speed, the clamping of a written
PWM to the board's min/max, and an extra trigger call. **None of the three
set paths is gated on it.** So the earlier conclusion, that a patched
driver "would probably still not give working PWM control", is too
pessimistic: in this version `hp_wmi_hwmon_is_visible` returns `0644` for
`hwmon_pwm` unconditionally, so installing it would expose `pwm1` on 8D2F,
and whether the firmware honours the query is then an empirical question
rather than a driver one.

**Confirmed on the hardware, 2026-09-02**, against a root daemon:

```
before        pwm1_enable=2 fan1=2093 fan2=1913
setMode max   ok
  +2s         pwm1_enable=0 fan1=2702 fan2=2536
  +4s         pwm1_enable=0 fan1=3312 fan2=3138
  +6s         pwm1_enable=0 fan1=3906 fan2=3737
setMode auto  ok
after         pwm1_enable=2 fan1=3654 fan2=3494   (already decaying)
```

So on a board that is *not* in `hp_wmi_feature_boards`, with the stock
kernel driver, max and auto genuinely drive the fans. This is the first
hardware write this project has ever made.

That also explains what the laptop's owner reports: *everything works
except setting the fan percentage*. Max and auto are reachable through
`pwm1_enable` on the stock driver right now; only the percentage needs
`pwm1`, which the running 7.2.2 driver does not expose. (The running driver
must therefore gate `hwmon_pwm_input` on the board table, unlike the source
above — the two cannot both describe the same code, and the kernel package
here ships no source to diff against.)

Three details that shape the port, all from the same file:

- **`pwm1 = 0` does not mean "fans off".** It is `HP_FAN_SPEED_AUTOMATIC`,
  and `hp_wmi_fan_speed_set` hands the fans straight back to the firmware.
  A curve point at 0 % would silently stop being a curve, so anything
  commanded is clamped to at least 1 (`curve::MIN_COMMANDED_PWM`).
- **`pwm1_enable = 1` on its own applies 128.** `priv->cpu_pwm` is
  initialised to 128 in `hp_wmi_setup_fan_settings`, so switching to manual
  without writing `pwm1` first pins the fans at 50 % chosen by nobody. The
  port writes the speed *before* the mode for exactly this reason.
- **The driver keeps max/manual alive itself**, every
  `KEEP_ALIVE_DELAY_SECS` = 90 s, and cancels that work on auto. The
  daemon's own re-assert is 60 s, comfortably inside both that and the
  ~120 s the EC takes to reclaim control.

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

## The test laptop has no per-key RGB keyboard

Settled on 2026-09-02, on the laptop, and it decides which of the two RGB
ports is worth writing:

| probe | result |
|---|---|
| `lsusb \| grep -i 0d62` | **nothing** — no HP Gaming Keyboard II |
| `ls /sys/devices/platform/hp-wmi` | present |
| `ls /proc/acpi/call` | absent — `acpi_call` is not installed |
| `ls /sys/class/leds` | no keyboard-backlight entry at all |

The whole USB bus is a camera, a Bluetooth radio and four root hubs, so
the per-key path (`driver.py`, `hidapi`, `0d62:54bf`) has nothing to talk
to here and should not be ported first.

That leaves the 4-zone lightbar path over `/proc/acpi/call`, which cannot
be *tested* until the `acpi_call` module is installed (`acpi_call-dkms` on
Arch; this machine's repositories also carry a prebuilt `acpi_call` for the
CachyOS kernel). Until then "no per-key device" is proven and "the 4-zone
interface answers" is not.

That path **is now ported** (`daemon/crates/rgb`, 2026-09-03) with its
buffer layout unit-tested and its firmware answer still unasked; the three
commands that ask it are at the end of
[`docs/04-rgb-porting-review.md`](../docs/04-rgb-porting-review.md).

## The RGB source on the USB stick is gone

Found on 2026-09-03, on the way to porting it. Every Python file in
`/run/media/paraguayo33/SAMSUNG USB/omen-rgb-linux-main/` is **zero bytes**,
all five of them stamped `sep 2 13:07`:

```
src/__init__.py  src/cli.py  src/driver.py  src/gui.py  src/lightbar.py
scripts/omen_cli.py  scripts/omen_gui.py          all 0 bytes
```

Everything else on the stick survived at its original `jul 24` timestamp —
`README.md`, `data/keys.json`, `examples/rainbow.py`, `setup.py`, the
licence. So this is a truncation of exactly the `.py` files that carry the
protocol, not a failing stick.

`docs/04-rgb-porting-review.md` was written from those files and says the
lightbar payload layout "is already documented in `lightbar.py`", which
was true when it was written and is not true of the copy on the stick.
**The port was written from upstream `main` instead**
(`raw.githubusercontent.com/arfelious/omen-rgb-linux/main/src/lightbar.py`),
which matches every quotation in the review — the `lstrip("b0x")`, the
two-branch `_detect_acpi_path`, the `struct.pack("<4sIII", b"SECU", …)`
header — so the review's findings are findings about the code that was
actually ported.

Two things follow. The stick is not a source of truth and should not be
cited as one again; and this is the second time the project has been
bitten by depending on an unpinned copy of somebody else's repository (the
first is §"The driver source is not vendored, on purpose", where the copy
on disk turned out to be an unidentifiable `main`). The conclusion there —
pin a release and the sha256 of each extracted file — applies here too, if
the per-key path is ever ported.

## `keys.json` really does contradict `set_all()`

The one half of the RGB review's first finding that could be settled
without the keyboard, read straight out of the surviving `data/keys.json`
on 2026-09-03:

- `backspace` is `{"offset": 60, "width": 8}`, so it covers indices 60–67,
  which includes the 62 and 63 that `set_all()` zeroes as "hardware
  alignment bytes".
- Indices 0, 1, 124 and 125 are assigned to no key, so the alignment
  theory holds for two of the three chunks.
- 100 keys, highest assigned index 181, in a 186-byte buffer.

So the contradiction is real and exactly where the review put it. Which of
the two is wrong still needs the hardware, and the per-key path is not
ported until it is settled — see the review's step 3.

## The RGB project has two unrelated hardware paths

Full review in [`docs/04-rgb-porting-review.md`](../docs/04-rgb-porting-review.md).
Short version: per-key RGB over USB HID (`0d62:54bf`) and a 4-zone lightbar
over ACPI share nothing, and which applies is not decided by the model
name — which is why the probes above had to be run rather than reasoned
about. On this laptop the answer is: not the per-key one.

That review also found: a contradiction between `set_all()` and
`data/keys.json` about backspace, an `lstrip("b0x")` that eats data bytes
(it takes a character set, not a prefix — verified), and that
`/proc/acpi/call` is a single global interface with no locking, which a
daemon must serialise **across modules** since the fan cleaner uses it too.

## Bugs the parity test caught

`tools/pyren-check.sh` and `pyren-check` are compared against shared
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

## Fn+P: the key is probably a scancode the kernel has no name for

`journalctl -k` on the test laptop, after somebody pressed something five
times:

```
atkbd serio0: Unknown key pressed (translated set 2, code 0xab on isa0060/serio0).
atkbd serio0: Use 'setkeycodes e02b <keycode>' to make it known.
```

Ten lines, one scancode, `0xab` (`e02b`), and **no other unmapped key on
this machine**. Which physical key it is has not been confirmed yet — that
needs somebody at the keyboard — but there is only one candidate.

What matters for the daemon is that this shape is *reachable*. `atkbd`
emits `EV_MSC/MSC_SCAN` for an unknown key even though it emits no
`EV_KEY`, so the scancode arrives on `/dev/input/event3` and can be bound
without `setkeycodes` and without a udev hwdb entry: **nothing about the
system has to change for the daemon to hear it**. Better still, a key with
no keycode is invisible to the compositor, so binding it here cannot
collide with anything the user has bound in Hyprland.

The cost is in the same fact. Press and release both emit the same bare
scancode with nothing to tell them apart, so one physical press looks like
two events; `hotkey.json`'s `repeatGuardMs` (300 ms) is what collapses
them. Mapping the scancode to a real keycode with `EVIOCSKEYCODE` would fix
that properly, and was rejected for now: it changes what every other
process on the machine sees, to save a debounce.

This is also why nothing is bound by default. The daemon learns the key
from the machine (`pyren-ctl hotkey learn`) rather than carrying a table of
keycodes per model — the board-list mistake, in a different costume.

## The first `hotkey learn` bound the touchpad

Worth writing down because the mistake was structural, not a typo. The
first real learn window on the test laptop caught **keycode 325 on
`SYNA32FF:00 06CB:CFC5 Touchpad`** - `BTN_TOOL_FINGER`, which the kernel
emits when a finger comes to rest on the trackpad. It is an `EV_KEY` press
by every measure the module was applying, so it was bound, and from then
on every touch of the trackpad advanced the power mode. The daemon log is
unambiguous:

```
hotkey: power mode Unlimited -> Eco
hotkey: power mode Eco -> Balanced
hotkey: power mode Balanced -> Performance
```

...four times in nine seconds, for as long as the trackpad was in use.

The lesson is that "reports `EV_KEY`" is not the same question as "is a
keyboard". Pointing devices report buttons through the same event type.
The fix is in two layers, and the first one is the real one:

1. The watcher opens a device only if `capabilities/key` has a bit set
   below `BTN_MISC` (0x100). The touchpad's mask is `e520 10000 0 0 0 0` -
   everything it can report is a button - so it is never opened, and its
   events cannot reach a learn window at all. The `HP WMI hotkeys` device
   passes, with its keys high in the keyboard range.
2. `setTriggers` refuses a `BTN_*` keycode anyway, for a keyboard with
   mouse buttons on it or a config file written by hand.

`pyren-ctl hotkey learn` now also prints how to undo itself, because the
device name it reports is how somebody notices they caught the wrong
thing.

## power-profiles-daemon cannot move this machine right now

Worth knowing before reading a widget that says the mode did not change,
because it is not the widget being wrong. `power.setMode` on the test
laptop currently fails, with the whole reason in the reply:

```
power-profiles-daemon: ... Failed to activate CPU driver 'intel_pstate':
Error writing '/sys/devices/system/cpu/cpufreq/policy11/energy_performance_preference':
Device or resource busy (26)
```

The machine has **no `/sys/firmware/acpi/platform_profile`** (see the board
8D2F section above — `hp-wmi` exposes no thermal-profile attribute here),
so power-profiles-daemon is the only mechanism `power` has left. And it is
being refused: `intel_pstate` is in active mode with the `performance`
governor, and in that state the kernel makes
`energy_performance_preference` read-only, so ppd's write returns `EBUSY`.

`powerprofilesctl get` says `power-saver` while the governor says
`performance`, which is what a half-applied profile looks like. What left
the governor there is not established — `cpupower.service` is disabled and
nothing else obvious is running. Setting it back to `powersave` should let
ppd work again, and would be the thing to try before concluding that a
power-mode change is broken in this project's code:

```sh
echo powersave | sudo tee /sys/devices/system/cpu/cpufreq/policy*/scaling_governor
```

The daemon reports all of this rather than swallowing it: `changed: false`
plus the `failed` list reaches the OSD, and the widget prints it under the
four modes.

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
