# Fan control: the driver will not set 0 RPM, and its minimum is not the fan's

*2026-09-11 — measured on board `8D2F` (patched `hp-wmi`), `fan_max_rpm` 5300.*

## 0 RPM is not a speed you can command

Neither the driver nor the EC will hold the fans at 0 RPM on a commanded
speed. Writing 0 through the `hp-wmi` hwmon interface is not "stop" — it is
`HP_FAN_SPEED_AUTOMATIC`, i.e. **hand the fans back to the firmware curve**
(`hp_wmi_apply_fan_settings`: `pwm == 0 → fan_speed[FAN] = HP_FAN_SPEED_AUTOMATIC`).

So the only way to reach 0 RPM is to switch to **auto** mode and let the
firmware shut the fans down on its own when temperatures are low. This is a
safety property of the hardware, not a Pyren limitation, and it is the
right one: a manual mode that could pin the fans off would be a way to cook
a laptop by leaving a slider down.

Pyren works with this rather than against it. In `manual` and `curve`, any
target below the floor PWM (`stopBelowPwm` in `fan.getStatus`) is handed to
the firmware — reported as `fansReleased` — so "0 %" on the curve still
means the fans stop when the machine is cool, without ever commanding a
zero the driver would reinterpret.

## The driver's minimum is the fan table's, not the fan's

The stock driver clamps every commanded speed to the slowest entry of the
firmware's fan table. On `8D2F` that entry is **1800 RPM**. Below it the
driver simply refuses to go — it returns the minimum to the EC.

Sweeping the fans with that clamp **lifted** (the patched driver's
`min_rpm_override` parameter) told a different story:

| commanded | fans did |
|---|---|
| 1800 → 1000 rpm | followed cleanly |
| 700 rpm | held |
| **600 rpm** | held exactly, both fans, across repeated runs |
| 500 rpm | dropped out — kicked back to ~600, sometimes stalled |

600 RPM is the real floor on this machine — a third of what the official
driver allows. On some runs 600 itself kicked once (700 → 1600 → 800 →
600) before settling, so the edge is not a hard number.

## What Pyren does with this

The patched driver **removes the official clamp**. Pyren then offers, in
**Settings → Fans → "Keep the official driver's minimum fan speed"**, a
toggle between:

- **the driver's minimum** — 1800 RPM here, the conservative default; and
- **Pyren's minimum** — the slowest speed a calibration sweep measured the
  fans *holding*, plus a **+100 RPM** margin. 600 held, so the floor
  becomes **700**.

Pyren's floor can never exceed the driver's (`pyren_floor` clamps it), and
a stall watch nudges it back up a step if the fans keep giving out near it,
leaving a note in the app's notification history.

## Is the official limit a mistake?

No. Running a fan below its stable threshold **can** be rough — small RPM
wobble, an occasional stall — and 1800 is a safe, universal number to ship.
But it is conservative, not correct: this machine's fans are steady down to
600. Lifting the limit is not harmful **as long as it is done carefully** —
which is what the calibration sweep, the +100 margin, the driver-floor
clamp, and the stall watch are all for.
