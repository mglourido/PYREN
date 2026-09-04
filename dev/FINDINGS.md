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

## The patched driver works on 8D2F — settled on 2026-09-04

Installed from the wizard's automatic mode, on the laptop, and it is the
first time the installer's execution path has ever run. It confirms the
inference in the section below: nothing gates the pwm path on more than
board-params being set at all.

| before | after |
|---|---|
| `fan1_input`, `fan2_input`, `pwm1_enable` | plus **`pwm1`** and **`pwm2`** |
| `pwm1` absent, so no speed could be commanded | `pwm1=102`, `pwm2=93`, `pwm1_enable=2`, fans reading 2100 / 1900 rpm |

What was actually done: the board came out of DMI (`8D2F`), was found
missing from `hp_wmi_feature_boards`, and was added to it with
`omen_v1_no_ec_board_params` — chosen from the model name, and the
conservative half of that choice because it reads no thermal profile back
from the EC. No fan ceiling was patched, since nothing had been calibrated;
the driver asks the firmware instead. The hooks strategy was used (no DKMS
on this machine), and `/etc/pacman.d/hooks/90-hp-wmi-omen.hook` is in place
for the next kernel upgrade. The stock module is at
`hp-wmi.ko.zst.bak` beside where it lived.

The `omen_v1_no_ec` choice turned out to be **inert on this board**, which
is stronger than "unconfirmed" - see the next section.

Three things the run taught, all fixed the same day:

- **The vendored tree is genuinely read-only.** `git status` on `driver/`
  is clean after an install: the plan stages into `/usr/src` and patches
  the copy.
- **The injected entry was mis-indented.** `find_sentinel` returned the
  offset of `{}`, so the new entry was spliced into the middle of the
  sentinel's line, after its tab. It compiled; it looked wrong in a file
  people paste into bug reports. It now inserts at the start of that line.
- **The daemon has to be restarted afterwards.** `FanModule` discovers its
  sysfs paths once, in its constructor, so a daemon that was running during
  the install goes on reporting "this driver exposes no pwm1" next to a
  `/sys` that has one. The wizard now says so and gives the command.

## The board-params variant is inert on most boards

Read out of `hp-wmi.c` on 2026-09-04, after the install raised the question
of whether `omen_v1_no_ec` was the right guess for 8D2F. It mostly is not a
question:

- **All four variants share one fan profile.** `victus_s_board_params`,
  `omen_v1_board_params`, `omen_v1_legacy_board_params` and
  `omen_v1_no_ec_board_params` all set
  `.fan_profile = &victus_s_fan_profile_params`. So the variant cannot
  affect fan control - the reason the driver is installed at all.
- **The thermal-profile half is only read on the Victus S path.**
  `hp_wmi_platform_profile_setup` tests `is_omen_thermal_profile()` first,
  then `is_victus_thermal_profile()`, and only then
  `is_victus_s_thermal_profile()`. A board in either of the first two
  tables takes that path, which reads EC `0x95` directly, and the variant's
  `thermal_profile` field is never consulted.

8D2F is in `omen_thermal_profile_boards`, so both apply: the variant
changes nothing here, and the `Unknown EC layout` warning cannot fire for
it either - that needs `victus_s_thermal_params`, whose `ec_tp_offset` is
`HP_EC_OFFSET_UNKNOWN`, and the omen variants use `0x95`, `0x59` and
"none" respectively.

Where it *is* live - a board in neither thermal table - the offset is now
**measured rather than guessed**. `installer.autodetect` reads EC `0x59`
and `0x95` through `ec_sys` and picks the variant whose offset is holding a
value `enum hp_thermal_profile_omen_v1` uses (`0x30`, `0x31`, `0x50`).
Neither holding one is a real answer too: the board keeps its profile
elsewhere, and `omen_v1_no_ec` is then correct rather than a fallback.

Deliberately not inferred from the Victus S values (`0x00`, `0x01`): they
are two of the commonest bytes in EC space, so matching on them would name
an offset from noise. And the probe reads only - `ec_sys` loads read-only,
and an EC register read is what the driver itself does on every profile
query.

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

## The driver source is vendored, as of 2026-09-04

`driver/` in this repository is now a verbatim copy of upstream's tree, and
`driver/README.md` carries its provenance. This reverses the earlier
decision, which is kept below because the reasoning against still stands —
it was simply outweighed.

**The argument against**: `hp-wmi.c` is a modified copy of a GPL-2 kernel
driver maintained in
[`omen-fan-control`](https://github.com/arfelious/omen-fan-control), and
copying it here means tracking their changes by hand.

**What outweighed it**: without it the installer could only work for
someone who already had the other project checked out. The driver path —
the whole reason the installer exists — did nothing on a fresh machine, and
the blocker it reported told the user to go and find a checkout. That is
not a smaller cost than a manual sync; it is the feature not existing.

The alternatives researched at the time, unchanged and still the better
end state if this is ever revisited:

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
- The copy vendored here is **upstream `main`, not any release**
  (`2eab8333…`, ≠ `v2.0.0`'s `f2c92786…`) — nobody knows which version it
  is, which is the problem pinning solves. Vendoring at least freezes it:
  the file is now in git history with a hash, rather than being whatever
  was on a USB stick that day.

Two rules keep the copy honest:

- **Nothing in Pyren writes to `driver/`.** An install stages the tree into
  `/usr/src` and patches the copy, so the snapshot stays pristine and a
  second install never starts from the first one's output.
- The patcher's tests check **every anchor it relies on against the real
  file here** (`patch::upstream_source_tests`, `autodetect::vendored_driver_tests`),
  so a constant or table renamed upstream fails `cargo test` rather than an
  install.

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
first is §"The driver source is vendored, as of 2026-09-04", where the copy
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

## The lights work, and what was in the way was our own read

Settled on 2026-09-04, on the laptop, with `acpi_call` finally installed
and loaded. Three separate things had to be untangled, and only one of them
was about the hardware.

### 1. `fs::read_to_string` on `/proc/acpi/call` returns nothing

This is the one that cost the most, and it had nothing to do with HP.

| how the reply was read | what came back |
|---|---|
| `fs::read_to_string` | **0 bytes** |
| one explicit `read()` into an 8 KiB buffer | **253 bytes, `PASS`, return code 0** |

Identical request bytes, same file, same process, run as root; the kernel
accepted all 308 bytes of the write in both cases. `/proc/acpi/call`
reports a size of zero like most of procfs, so `read_to_string` has no hint
to size its buffer with and opens by probing with a very small one — and
this interface answers a small read with *nothing at all* rather than with
the first few bytes. `read_to_string` reads that zero as end-of-file and
hands back an empty string.

Every symptom of it was a lie about the machine. An empty reply is not
`PASS`, so it was reported as the firmware refusing, and a refusal reads as
"this laptop cannot do it". It is why every lighting dialect was reported
refused, and — through the same file — why the fan cleaner reported "this
machine has no fan cleaner". Fixed in `pyren_core::acpi::read_reply`, with
the reasoning in a comment there so nobody reaches for `read_to_string`
again.

**The general lesson:** a procfs file that reports size 0 cannot be read
with `read_to_string` unless you know its read handler tolerates a small
buffer.

### 2. This machine is `fourZone`, and `lightbar` is a false positive

Once replies arrived, the raw `FOURZONE_COLOR_GET` answer had the
keyboard's actual colours in it, at exactly the offset both reference
kernel drivers give (25 + 3 × zone):

```
state[25..28] = 0f 84 fa   zone 0   blue
state[28..31] = 71 0f fa   zone 1   purple
state[31..34] = f9 35 0f   zone 2   orange
```

The `lightbar` dialect — the one this module was originally ported as —
**also answers `PASS` on this machine and does nothing**: writes are
accepted, the lights do not change, and its read reports all four zones
black. So "the firmware accepted it" is not evidence that a dialect works,
which is the argument for trying more than one and for the manual override.
`fourZone` is tried first of the two for this reason.

### 3. `acpi_call` truncates the reply, and zone 4 is the casualty

`acpi_call` renders a buffer reply as the text `{0x50, 0x41, …}` into a
fixed result buffer of a few hundred bytes, so the firmware's 128-byte
answer arrives as its first **34** bytes. Zones 0–2 fit; zone 3 starts at
byte 34 and does not. Consequences — all of them live at the time, and all
of them gone since `kernelZones` started answering, two sections down:

- `rgb read` always reports zone 4 as black. The colour written to it is
  real — the lights show it — but it cannot be read back.
- A write pads the unseen tail with zeros. Bounded, but a guess: everything
  visible before the colours is zero apart from `state[0] = 3`.

The way out is the `kernelZones` dialect, which needs no `acpi_call` and
has no such limit. This kernel's `hp-wmi` does not publish those files;
`OmenLinux/omen-rgb-keyboard` is an out-of-tree module that would. Two
things about that module turned out not to be what this paragraph first
assumed — see "What `omen-rgb-keyboard` actually costs" below.

## Full speed on this machine is 5200 rpm — measured 2026-09-04

`fan calibrate`, run against the hardware for the first time. It is the
number the curve's hysteresis and the installer's `cpuMaxRpm`/`gpuMaxRpm`
patch both wanted, and until now every one of them was working from
whatever ceiling the firmware volunteered.

```
baseline 2400 rpm (idle, mode auto)
  1s  2600 / 2400      9s  4800 / 4600
  2s  2900 / 2700     10s  5100 / 4900
  3s  3200 / 3000     11s  5200 / 5100
  4s  3500 / 3300     12s  5200 / 5200
  5s  3800 / 3600     13s  5200 / 5200
  6s  4100 / 3900     14s  5200 / 5200
  7s  4300 / 4100     15s  5200 / 5200
  8s  4600 / 4400     16s  5200 / 5200
fan1Max 5200, fan2Max 5200, fanMax 5200
restoredMode auto, restoreError null
```

Three things the trace says that the single number does not:

- **The ramp is linear and slow** — about 290 rpm/s, sixteen seconds from
  idle to the ceiling. Anything that reads an rpm right after commanding
  `max` and calls it the maximum is reading the ramp, not the ceiling.
  This is the argument for calibration existing at all.
- **The two fans are not in step.** `fan1` leads `fan2` by 200 rpm the
  whole way up and they land on the same number. So the lag is the ramp,
  not different hardware, and taking the max of the two is right.
- **It put the mode back.** `restoredMode: auto` with no error, which is
  the half of the routine that had never been exercised against a real
  `switchMode`.

## What `omen-rgb-keyboard` actually costs — read 2026-09-04

Read out of the clone before installing it, and both findings contradict
what §1.2b of the TODO assumed.

**It does not publish under `hp-wmi`.** `DRIVER_NAME` is
`"omen-rgb-keyboard"` and `platform_device_register_simple(DRIVER_NAME, …)`
registers under that, so the files are at
`/sys/devices/platform/omen-rgb-keyboard/rgb_zones/zone00…03`. The
variable holding the device is still called `hp_wmi_platform_dev`, which
is probably why the older path is the one written down everywhere. Our
`kernelZones` dialect had `/sys/devices/platform/hp-wmi/rgb_zones`
hardcoded, so installing the module and expecting `rgb probe` to notice
would have found nothing and taught us nothing. `dir()` now searches both.

**It wants `hp_wmi` gone.** The repo ships `hp_wmi-blacklist.conf`
(`blacklist hp_wmi`) and its README says to `modprobe -r hp_wmi` and
regenerate the initramfs, on the grounds that the two fight over WMI
events. That is not a small ask here: every fan control PYREN has goes
through `hp-wmi`'s hwmon, including the 5200 rpm above, and this machine's
`hp-wmi` is the *patched* one that the installer put there to get `pwm1`
at all. Taking it out to read one more RGB zone trades the project's main
feature for a colour.

So the experiment worth running is the one the module's own documentation
says not to: load both, then check `hp-wmi`'s `pwm1` still answers. **It
does** — see the next section.

## `kernelZones` works, and `hp_wmi` did not have to go — 2026-09-04

`omen-rgb-keyboard` 1.4 installed through DKMS and loaded **without**
removing or blacklisting `hp_wmi`, against its own README's instructions.
The result is the good one on both counts:

| | |
|---|---|
| `lsmod` | `omen_rgb_keyboard` **and** `hp_wmi` loaded together |
| `hp-wmi` `pwm1` | still answers (`127`), fan control untouched |
| zone files | `/sys/devices/platform/omen-rgb-keyboard/rgb_zones/zone00…03` |
| `rgb probe` | `kernelZones yes - answered a read of all four zones` |
| `rgb read` | `#0f84fa #710ffa #f9350f #f9350f` |
| `rgb get` | `dialect kernelZones (chosen automatically)` |

**Zone 4 reads back.** `#f9350f` where `acpi_call`'s truncated reply could
only ever report black. The read half of §"3. `acpi_call` truncates the
reply" is closed, and with it the zero-padded write: `kernelZones` writes
one file per zone and never builds a state buffer to guess the tail of.

Two things worth keeping from this:

- **The README's conflict did not happen here.** It says the two modules
  fight over WMI events and tells you to blacklist `hp_wmi`. On this board
  they coexist, and since our fan control *is* `hp-wmi`'s hwmon, that was
  the difference between the dialect being usable and being unaffordable.
  Not a general claim — one board, one kernel — but it is the board we
  have, and the check is cheap to repeat (`tools/try-kernel-zones.sh`
  rolls back by itself if `pwm1` ever stops answering).
- **The auto-pick needed no help.** `kernelZones` is first in `ORDER`, it
  became reachable, and the daemon chose it with nothing pinned — no
  restart required either, since the probe runs per call.

## `hotkey press` is safe to test with — 2026-09-04

Two properties of `pyren-ctl hotkey press`, found while using it to raise
the OSD. Neither is about whether the widget works — it does, and did
before — but both matter for reading the `hotkey learn` runs:

- **It does not cycle the power mode.** The mode was `eco` before and
  after. It raises the widget and lets the *selection* do the switching,
  so it can be fired at any time without changing the machine.
- **It does not increment `presses`.** That counter stayed at 0 across a
  press. So it counts real keys off an evdev device and not synthetic
  ones, which makes it trustworthy evidence that Fn+P arrived — a
  non-zero `presses` cannot have come from us.

## GPU MUX switching needs no `supergfxctl` — read and confirmed 2026-09-04

Read out of `driver/hp-wmi-omen/hp-wmi.c` while scoping TODO §2.1's GPU
switching decision, before writing anything: the driver this project
already patches and installs for fan control defines
`HPWMI_GRAPHICS_MUX_QUERY` (`0x52`) and exposes it as a plain `RW` sysfs
attribute, `gpu_mux_mode`. `supergfxctl` is not installed on this machine
and was never seriously in the running once that was known — it would
have been a second daemon doing the same ACPI-WMI round trip a file this
project already owns can do directly.

```
$ cat /sys/devices/platform/hp-wmi/gpu_mux_mode
0
```

`0` is `hybrid`, matching the app's own default. Confirmed against the
daemon after `pyren-gpu` was built: `pyren-ctl gpu get` reads the same
`hybrid`, restarting the daemon (a debug build) left fans and lighting
untouched, and `core.capabilities` / the compatibility line both picked up
`gpu` without any hand-written list to update — it comes from the
registry.

Two things read out of the source that are easy to get backwards:

- **The bitmask constants (`HPWMI_MUX_MODE_HYBRID = BIT(1)`, etc.) are not
  the wire format.** They exist only so the kernel can check a requested
  mode against a supported-set query before writing it. The byte actually
  read from and written to `gpu_mux_mode` is a plain index — `0` hybrid,
  `1` discrete, `2` optimus, `3` uma — and read/write agree on it.
- **There is no userspace query for which modes a board supports.** The
  capability check happens inside the kernel, on write, against a
  design-data query with no sysfs file of its own. So a mode the firmware
  refuses can only be discovered by asking it to switch and reading
  `EOPNOTSUPP` back — unlike `rgb`, which probes every dialect with a read
  that changes nothing first.

**`gpu.setMode` has not been run.** It changes the session's card and
needs a logout or reboot to take effect, which is exactly the kind of
write this pass should not fire blind. `getStatus` is the half that is
confirmed; trying an actual switch is next.
