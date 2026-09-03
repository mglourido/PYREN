# RGB module: review of `omen-rgb-linux` before porting

Notes from reading `omen-rgb-linux` with a view to porting it as the `rgb`
daemon module. **Nothing here was tested against hardware**: it was written
on an ASUS desktop with no HP keyboard, no `hp-wmi` and no
`/proc/acpi/call`, so everything below comes from reading the code and the
data files. Items marked ⚠ are things to check before trusting them.

Since then the development machine has become the OMEN laptop itself, which
answers the review's central question — see "Suggested porting order" at
the end. The source is not in this repository; the copy on the USB stick
(`/run/media/paraguayo33/SAMSUNG USB/omen-rgb-linux-main/`) has been
**truncated to zero bytes** and is no longer readable — see
`dev/FINDINGS.md` §"The RGB source on the USB stick is gone".

## What has been done since (2026-09-03)

The review has been acted on. Steps 1 and 2 of the porting order below are
implemented as the `rgb` module (`daemon/crates/rgb`, documented in
[`01-ipc-protocol.md`](01-ipc-protocol.md) §"`rgb` module"); step 3 is not,
on purpose.

| finding | what happened |
|---|---|
| 1. `set_all()` vs `keys.json` on backspace | **Confirmed against the data**, not fixed: `keys.json` really does put `backspace` at offset 60 width 8, covering 62 and 63. Not ported — see step 3. |
| 2. `lstrip("b0x")` eats data bytes | **Fixed** in the port (`lightbar::parse_bytes` strips the prefix once), with the two examples from this review as a test. |
| 3. `_detect_acpi_path` is dead code | **Fixed**: the path is a plainly-named constant with a comment saying what would make it a probe again. |
| 4. `/proc/acpi/call` needs a cross-module lock | **Done**, in `pyren_core::acpi` — not in the `rgb` module, for the reason this review gives. |
| 5. root, and a udev rule would be better | Not applicable yet: only the lightbar is driven, and that needs root whatever the udev rules say. Still true for the per-key path when it lands. |
| 6. behaviour worth keeping | Kept where it applies. The lightbar's `readZones` really does ask the firmware, unlike the keyboard's write-only buffer, and the module docs say so out loud so the two are not read as the same question. |

**Still not done, and it is the only thing left:** none of this has been
run against a light strip. `/proc/acpi/call` does not exist on the
development laptop because `acpi_call` is not installed. Everything that
can be tested without it is (the buffer the port builds, the replies it
accepts, the probe on a machine that has neither path), so the untested
remainder is exactly one thing: the firmware's own answer.


## There are two unrelated hardware paths

This matters more than any individual bug, because they need different
code, different privileges and different detection:

| | Per-key RGB | 4-zone lightbar |
|---|---|---|
| Source | `src/driver.py` | `src/lightbar.py` |
| Transport | USB HID, `hidapi` | ACPI-WMI via `/proc/acpi/call` |
| Device | HP Gaming Keyboard II, USB `0d62:54bf` | `hp-wmi` + `acpi_call` module |
| Target | "HP Omen **Max** 16" | OMEN laptops with the bottom light strip |

**Which one applies to a given laptop is not decided by the model name**,
so the module has to probe for both. On the target machine:

```sh
lsusb | grep -i 0d62          # per-key keyboard present?
ls /sys/devices/platform/hp-wmi   # WMI interface present?
ls /proc/acpi/call            # acpi_call loaded? (needs the acpi_call package)
```

The screenshots this project's UI was built from show a zoned keyboard, so
the 4-zone path is the more likely one — but that needs confirming before
either path is ported, because they share nothing.

## Findings

### 1. `set_all()` and `set_key_color()` disagree about two bytes ⚠ *(confirmed)*

`driver.py`'s 186-byte per-channel buffer is sent as three 62-byte chunks.
`set_all()` deliberately zeroes the first two bytes of each chunk,
calling them "hardware alignment bytes":

```python
for chunk_idx in range(3):
    self.channels[ch_id][chunk_idx * 62] = 0
    self.channels[ch_id][chunk_idx * 62 + 1] = 0
```

That zeroes buffer indices 0, 1, 62, 63, 124, 125. But `data/keys.json`
assigns **`backspace` to offsets 60–67**, which covers 62 and 63. Indices
0/1 and 124/125 are indeed unassigned, so the "alignment" theory holds for
two of the three chunks and is contradicted for the middle one.

Note also that `apply()` builds the report as
`[channel_id][chunk_idx][62 payload bytes]` — the header lives in
`report[0..2]`, *not* in the buffer — so on the face of it all 186 bytes
are payload and the zeroing is a leftover from an earlier layout.

**The `keys.json` half of this is now confirmed** (2026-09-03, read
straight out of the file): `backspace` is `{"offset": 60, "width": 8}`, so
it covers 60–67 and therefore 62 and 63. Indices 0/1 and 124/125 are
assigned to no key at all, and the highest assigned index is 181 across
100 keys. So the contradiction is real and it is exactly where the review
put it.

One of the two is wrong. **A concrete test on the laptop:** set everything
white with `static`, and look at backspace; then run
`set-key backspace ffffff`. If backspace has a dark segment in the first
case and is uniform in the second, `set_all()` is blanking real LEDs. If
both look identical, the key map's `backspace` entry is the wrong shape.

### 2. `lstrip("b0x")` eats data bytes *(fixed in the port)*

`lightbar.py:_parse_acpi_response_bytes` strips a prefix with:

```python
raw_hex = clean_res.lstrip("b0x")
```

`str.lstrip` takes a **character set**, not a prefix, so it removes *every*
leading `b`, `0` or `x`:

```
'0xb0b0aa'.lstrip('b0x')  ->  'aa'          # three bytes of data gone
'0x0050415353'            ->  '50415353'    # leading zero byte gone
```

It should be `removeprefix("0x")`. This is in the fallback branch used only
when the `0x..` regex above it matches nothing, so it is latent rather than
always wrong — but when it does fire it silently returns the wrong colours.

### 3. `_detect_acpi_path` is dead code *(fixed in the port)*

Both branches return the same `"\\_SB.WMID.WMAA"`, so the `os.path.exists`
check does nothing. Harmless, but it reads as though the path is probed
when it is hardcoded. In the port, either probe properly or hardcode it
plainly.

### 4. `/proc/acpi/call` is a single global interface *(done)*

Every call is write-then-read on one file with no locking. Two processes
using it concurrently will read each other's responses. The Python CLI gets
away with this because it is one short-lived process; **a daemon must
serialise all `acpi_call` use behind one mutex**, and that includes the fan
module, which the source project also drives through `acpi_call` for the
fan cleaner. This is a cross-module constraint, so the lock belongs
somewhere shared rather than inside the `rgb` module.

### 5. Root is required, and a udev rule would be better

The README's answer to permissions is `sudo` everywhere. In this project
the daemon is already root, so the port doesn't inherit the problem — but
if the keyboard is ever driven from the unprivileged app, a udev rule
granting the `input`/`plugdev` group access to the `0d62:54bf` hidraw
interface is the right fix rather than escalating the whole app.

### 6. Behaviour that is correct and worth keeping

- `get_colors()` on the keyboard returns the **driver's buffer**, not
  hardware state, and says so — the HID lighting interface is write-only.
  The port should keep that distinction visible rather than implying the
  hardware was queried.
- The interface-number-3 filter in `hid.enumerate` is how the lighting
  endpoint is told apart from the keyboard's input endpoints. Don't
  "simplify" it to the first match.
- The double-write of the commit report (`0x0a`) is marked mandatory. It
  looks like a workaround for a firmware quirk; keep it and keep the
  comment.

## Suggested porting order

The question this review could not answer has since been answered on the
hardware (2026-09-02, see `dev/FINDINGS.md` §"The test laptop has no
per-key RGB keyboard"): **`lsusb` finds no `0d62` device**, and
`/sys/class/leds` has no keyboard-backlight entry either. The per-key path
has nothing to talk to on this machine, so:

1. ✅ **Done.** Probe both paths and report what exists
   (`rgb.getCapabilities`), the way the `system` module already reports
   what it found the machine able to do. Probing rather than matching a
   model name is the rule everywhere in this project now, and this module
   is no exception.
2. ✅ **Written, not yet confirmed.** Port the 4-zone lightbar path. It is
   ~250 lines, needs no HID dependency, and the payload layout is
   documented in `lightbar.py`.

   The order this step suggested was *install `acpi_call`, confirm the
   lightbar answers, then write the module.* It was written the other way
   round, and that is a deliberate change rather than a shortcut: the
   protocol is a 144-byte buffer whose every field is a guess inherited
   from upstream, and a hand-typed `echo` into `/proc/acpi/call` proves
   only that one hand-typed buffer was refused. The module builds the
   buffer, its unit tests pin every field, and `pyren-ctl rgb probe` /
   `rgb set` are then the way to put the question to the firmware — with
   the answer landing somewhere a second person can reproduce.

   So what remains is unchanged in substance and smaller in size: install
   `acpi_call` and see whether the firmware says `PASS`.
3. ⏸ **Not done, on purpose.** Port the per-key path only if a `0d62:54bf`
   device turns up on some other machine, and settle finding 1 first — the
   key map is the whole value of that path and it should not be ported with
   a known inconsistency in it. Finding 1's `keys.json` half is now
   confirmed; the `set_all()` half needs the keyboard.

Whichever lands, `/proc/acpi/call` needs a **cross-module** lock, since the
fan cleaner will use it too. That belongs in `core` or a new shared crate,
not inside the `rgb` module. ✅ It is `pyren_core::acpi`, and the fan
cleaner should reach for it rather than opening the file itself.

## Confirming it on the hardware

The whole remaining question, in three commands. `acpi_call` is packaged
on this machine's distribution — both a prebuilt `acpi_call` for the
CachyOS kernel and the source `acpi_call-dkms`, whose build the installed
`linux-cachyos-headers` satisfies:

```sh
sudo pacman -S acpi_call        # or acpi_call-dkms
sudo modprobe acpi_call         # the daemon will also do this on demand
ls /proc/acpi/call              # it exists now

cd daemon && sudo -E cargo run -p pyren-daemon     # terminal 1
cargo run -q -p pyren-ctl -- rgb probe             # terminal 2
```

`rgb probe` prints one line per path. If the lightbar line says the
firmware answered, `rgb set '#ff9900'` is the first write, and
`rgb read` asks the firmware what it thinks the zones are — which is the
check that the *payload* was understood, not merely accepted.

Either answer is worth writing into `dev/FINDINGS.md`. "The firmware
refused" is as much a result as "the strip turned orange", and it is the
one that stops the next person re-deriving this.
