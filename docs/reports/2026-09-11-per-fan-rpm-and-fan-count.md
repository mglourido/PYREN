# Per-fan RPM readout, and how many fans this machine has

*2026-09-11 — measured on the OMEN test laptop (patched `hp-wmi`, `hwmon9 = hp`).*

## What changed

Performance control now shows, under the headline fan speed (the single
figure sent to the controller), each fan's own tachometer, labelled by the
cooler it drives:

```
        2100 RPM
   CPU fan 2100 RPM   ·   GPU fan 1900 RPM
```

- **Daemon** — `fan.getStatus` gained a `fans` array: one entry per
  `fan?_input` file the `hp` hwmon exposes, `key` being `"cpu"` for `fan1`
  and `"gpu"` for `fan2` (the OMEN wiring the calibration already assumes),
  each `rpm` / `isReverse` decoded through the same reverse-bit encoding as
  `fanRpm`. Empty on a machine with no `hp` hwmon; one entry on a single-fan
  machine. `fanRpm` (`max(fan1, fan2)`) is unchanged and stays the summary.
- **App** — a new setting, **Show each fan's speed in Performance control**
  (`perFanRpm`, on by default), gates the list. It only renders when the
  machine reports more than one fan, so a single-fan machine sees nothing
  extra.

## "There should be three fans"

The laptop was believed to have three fans. It has **two**.

### What the kernel exposes

| hwmon | driver | fans |
|---|---|---|
| `hwmon9` | `hp` (patched hp-wmi) | `fan1_input 2100`, `fan2_input 1900` |
| `hwmon1` | `acpi_fan` (`\_SB.FAN0`) | `fan1_input 2118` |
| `hwmon2` | `acpi_fan` | — (no tachometer) |

There is a single ACPI fan object (`PNP0C0B:00 → \_SB_.FAN0`). The
`acpi_fan` reading of **2118** tracks the `hp` CPU fan (**2100**): it is the
same physical fan seen through the generic ACPI interface, not a third one.

The patched `hp-wmi` driver hardcodes two hwmon fan channels
(`HWMON_CHANNEL_INFO(fan, HWMON_F_INPUT, HWMON_F_INPUT)`, `CPU_FAN 0` /
`GPU_FAN 1`) and a fan table that only carries `cpu_rpm` / `gpu_rpm`. There
is no `fan3_input` by construction.

### What the firmware says

Queried directly over `/proc/acpi/call` (`\_SB.WMID.WMAA`, command group
`GM = 0x20008`), the same path the fan cleaner uses. Replies are the `PASS`
signature, a `u32` return code of `0`, then the data bytes.

| Query | Command type | Data | Decoded |
|---|---|---|---|
| Fan count | `0x10` | `02 00 00 00` | **2** |
| Fan speed, index 0 | `0x11`, payload `00` | `24 00 08 41` | `0x0841` = 2113 rpm (CPU) |
| Fan speed, index 1 | `0x11`, payload `01` | `24 00 07 80` | `0x0780` = 1920 rpm (GPU) |
| Fan speed, index 2 | `0x11`, payload `02` | `24 00 07 6f` | `0x076f` = 1903 rpm |
| Fan speed, index 3 | `0x11`, payload `03` | `24 00 07 70` | `0x0770` = 1904 rpm |

RPM is `(data[2] << 8) | data[3]`, matching the reference driver's
`hp_wmi_get_fan_speed`. Indices 0 and 1 line up exactly with the `hp`
hwmon's CPU and GPU readings.

`FAN_COUNT_GET_QUERY` returns **2**. Indices 2 and 3 answer `PASS` but with
junk — a stale GPU-ish value repeated — because the firmware does not
range-check the index. They are not real fans.

## Conclusion

This machine has two fans (CPU + GPU). The per-fan breakdown as shipped —
`fan1 → CPU`, `fan2 → GPU`, read from the `hp` hwmon — is complete and
correct for it. No WMI fan-count / per-index path is needed; if a genuinely
three-fan OMEN turns up later, `FAN_COUNT_GET_QUERY` + `FAN_SPEED_GET_QUERY`
over the existing `acpi::wmi_call` helper is the way to reach the third,
and this report has the wire format.
