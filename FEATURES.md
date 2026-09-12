# Hardware-related features

- **Automatic switching**, per power source, between a configurable preferred mode and a refined mode under load/idle/heat/low-battery (`preferredOnBattery`: Eco or Balanced; `preferredOnMains`: Balanced or Performance), with a manual-override baseline, dead-band return, and an Advanced Settings panel for the tuning
- **Manual power profile control** (Eco, Balanced, Performance, Unlimited)
- **OS power manager support** beyond power-profiles-daemon: speaks TLP (1.8+ direct or via tlp-pd) and auto-cpufreq too, all through non-activating D-Bus/CLI calls so reading state or running `pyren-check` never starts a power manager the user didn't choose
- **External change detection** — the daemon watches firmware profile, EPP, turbo and package limits once a second and notices when something outside pyren (Fn+P, a desktop's battery menu, another power manager) moves them, with no arbiter needed
- **GPU overclocking** (memory frequency, core clock; ability to set a minimum for both)
- **Power tuning** for the Performance and Unlimited profiles
- **Fan control modes**:
  - **Max** — fans run at the maximum configured RPM
  - **Auto** — managed by the active performance profile (firmware has its own curve per profile)
  - **Manual** — user sets a fixed fan speed (%) that stays constant
  - **Curve** — user defines a custom fan curve
- **Lower minimum fan speed** — the patched driver lets the fans run below the slowest entry of the firmware's fan table (which the stock driver enforces as a floor); a calibration measures how slow the fans on this machine can actually hold, a setting chooses that measured minimum or the driver's, and below it the firmware takes over and stops the fans when the machine is cool
- **Per-fan RPM readout** — under the single fan speed sent to the controller, Performance control can list each fan's own tachometer (CPU fan, GPU fan) so the user sees where every reading comes from; on by default, toggled in Settings, and hidden on single-fan machines
- **Automatic minimum-speed correction** — the daemon watches for the fans stalling near that minimum and nudges it up on its own, with an entry in the app's notification history
- **Fan cleaning mode** (fans spin in reverse to help clear out dust)
- **Keyboard RGB control**, with a lighting-effects engine (breathing, spectrum, rainbowWave, wave, fade), power-on/off colour sweeps, brightness (paused, not faked, at zero), and effects/brightness throttled by battery and lid state; up to 15 saved lighting configurations, stored in their own config file
- **Graphics switching** (toggle between "Integrated only" [iGPU only], "Hybrid" [switches between iGPU and dGPU], and "Discrete" [dGPU only])
- **Key remapper** (create macros, reassign keys)
- **Network booster** (currently only "Disable" and "Automatic"; per-process control isn't properly supported yet)
- **Quick-access widget**, triggered via keyboard shortcut without the app running, for fast switching between performance profiles — and, once enabled in Settings, the fan control modes too (auto/max/manual/curve, with a slider for the manual speed; the curve is still edited in the app)

## Other features

- **Sync OS power profile** with the hardware power profile simultaneously
- **App permissions manager**
- **Feature compatibility checker**
- **Safe installer** for the patched Linux kernel driver `hp-wmi`, supporting automatic (recommended) or manual installation (for testing or as a fallback if automatic install fails)
- **App update checker**
- **Notification centre** — a bell in the header opens a centre-screen history of daemon events (currently the automatic fan-floor corrections); live events also fire an OS notification

## Other app features

- **System monitor** (temperatures [GPUs, CPU, chassis], usage %, RAM, disk, network, top processes); includes an advanced mode with per-core CPU/GPU monitoring and more detailed RAM usage analysis, with small graphs for each metric
- **Temperature units**: Celsius or Fahrenheit
- **Language manager** (`i18next`)
- **Boot options**
- **UI themes**: Dark, Light, Dracula, Tokyo Night, Zero Two, Doki: Essex, Cobalt2, Ayu
