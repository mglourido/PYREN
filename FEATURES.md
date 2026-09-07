# Hardware-related features

- **Automatic switching** between Eco and Balanced performance modes
- **Manual power profile control** (Eco, Balanced, Performance, Unlimited)
- **GPU overclocking** (memory frequency, core clock; ability to set a minimum for both)
- **Power tuning** for the Performance and Unlimited profiles
- **Fan control modes**:
  - **Max** — fans run at the maximum configured RPM
  - **Auto** — managed by the active performance profile (firmware has its own curve per profile)
  - **Manual** — user sets a fixed fan speed (%) that stays constant
  - **Curve** — user defines a custom fan curve
- **Fan cleaning mode** (fans spin in reverse to help clear out dust)
- **Keyboard RGB control**
- **Graphics switching** (toggle between "Integrated only" [iGPU only], "Hybrid" [switches between iGPU and dGPU], and "Discrete" [dGPU only])
- **Key remapper** (create macros, reassign keys)
- **Network booster** (currently only "Disable" and "Automatic"; per-process control isn't properly supported yet)
- **Quick-access widget**, triggered via keyboard shortcut without the app running, for fast switching between performance profiles and fan control

## Other features

- **Sync OS power profile** with the hardware power profile simultaneously
- **App permissions manager**
- **Feature compatibility checker**
- **Safe installer** for the patched Linux kernel driver `hp-wmi`, supporting automatic (recommended) or manual installation (for testing or as a fallback if automatic install fails)
- **App update checker**

## Other app features

- **System monitor** (temperatures [GPUs, CPU, chassis], usage %, RAM, disk, network, top processes); includes an advanced mode with per-core CPU/GPU monitoring and more detailed RAM usage analysis, with small graphs for each metric
- **Temperature units**: Celsius or Fahrenheit
- **Language manager** (`i18next`)
- **Boot options**
