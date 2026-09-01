# RGB module: review of `omen-rgb-linux` before porting

Notes from reading `omen-rgb-linux` (`../omen-rgb-linux-main`) with a view
to porting it as the `rgb` daemon module. **Nothing here was tested against
hardware** — the development machine is an ASUS desktop with no HP
keyboard, no `hp-wmi` and no `/proc/acpi/call`, so everything below comes
from reading the code and the data files. Items marked ⚠ are things to
check on the laptop before trusting them.

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

### 1. `set_all()` and `set_key_color()` disagree about two bytes ⚠

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

One of the two is wrong. **A concrete test on the laptop:** set everything
white with `static`, and look at backspace; then run
`set-key backspace ffffff`. If backspace has a dark segment in the first
case and is uniform in the second, `set_all()` is blanking real LEDs. If
both look identical, the key map's `backspace` entry is the wrong shape.

### 2. `lstrip("b0x")` eats data bytes

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

### 3. `_detect_acpi_path` is dead code

Both branches return the same `"\\_SB.WMID.WMAA"`, so the `os.path.exists`
check does nothing. Harmless, but it reads as though the path is probed
when it is hardcoded. In the port, either probe properly or hardcode it
plainly.

### 4. `/proc/acpi/call` is a single global interface

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

1. Probe both paths and report what exists (`rgb.getCapabilities`), the way
   the `system` module already reports hardware identity. This is testable
   on the laptop immediately and settles the per-key vs 4-zone question.
2. Port the 4-zone lightbar path first if that is what the hardware has: it
   is ~250 lines, needs no HID dependency, and the payload layout is
   already documented in `lightbar.py`.
3. Port the per-key path only if the `0d62:54bf` device is present, and
   settle finding 1 first — the key map is the whole value of that path and
   it should not be ported with a known inconsistency in it.
