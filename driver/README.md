# `driver/` — the patched `hp-wmi`, vendored

A **verbatim, unmodified** copy of the kernel-driver tree from
[`omen-fan-control`](https://github.com/arfelious/omen-fan-control)
(`src/omen_fan_control/data/driver/`). It is the one thing in this
repository that is not Pyren's own code, and it deliberately stays in C:
`hp-wmi.c` is a backport of a Linux kernel driver, it is built as a kernel
module by the kernel's own build system, and rewriting it in Rust would
mean maintaining a fork of a driver whose upstream is still moving.

Pyren's installer (`daemon/crates/installer/`) is the Rust part: it decides
*whether* this needs installing, patches two constants and at most one
table, adds two module parameters, and drives DKMS or the distribution's
kernel hook.

The parameters are the one addition that is not a value substitution, and
they are here because the constants alone do not work: `OMEN_CPU_MAX_RPM`
and `OMEN_GPU_MAX_RPM` are the driver's *last* fallback, overwritten during
probe by whatever the firmware answers, so on most boards a measured fan
ceiling patched into them is silently discarded. `cpu_max_rpm_measured` and
`gpu_max_rpm_measured` are applied after those queries instead, which also
means a calibration can pin its result without anything being rebuilt. See
`dev/FINDINGS.md` §"The patched fan ceiling was never reaching the driver".

## Why it is here at all

It used to be left out, on the reasoning that copying a GPL-2 kernel driver
means tracking upstream's changes by hand. The cost of leaving it out was
larger: the installer could only work for someone who already had the other
project checked out, so the driver path — the entire reason the installer
exists — did nothing on a fresh machine. `dev/FINDINGS.md` has the
alternatives that were weighed (a pinned release fetched at build time is
still the option that removes the maintenance burden entirely).

## Provenance

| | |
|---|---|
| Upstream | `github.com/arfelious/omen-fan-control` |
| Taken from | upstream `main`, **not** a tagged release |
| `hp-wmi.c` sha256 | `2eab833344d4ff7ca52a07d7fc5a6124c2285028ac0cdf7defc772fbad833d62` |
| Copied on | 2026-09-04 |

`hp-wmi.c` and `hp-wmi.c.orig` are byte-identical here, which is the state
upstream ships: `.orig` is the pristine snapshot the patcher always works
from, so patching twice gives the same result as patching once.

## Licensing

`hp-wmi.c` is a modified copy of a Linux kernel driver. Its
`SPDX-License-Identifier` and its `MODULE_LICENSE` both say
**`GPL-2.0-or-later`** — GPL v2 *or, at your option, any later version*.
That "or later" is what makes it distributable inside Pyren: GPL-2.0-or-later
is upward-compatible with **GPL v3**, the licence of the surrounding project.

This file is **not relicensed**. It keeps its own SPDX header and its own
copyright notices:

- Copyright (C) 2008 Red Hat `<mjg@redhat.com>`
- Copyright (C) 2010, 2011 Anssi Hannula `<anssi.hannula@iki.fi>`
- portions based on `wistron_btns.c`, Copyright (C) 2005 Miloslav Trmac,
  Bernhard Rosenkraenzer, Dmitry Torokhov

Do not strip those, and do not change the SPDX tag. When the installer
patches its staged copy at build time, the result is a modified work that
stays under GPL-2.0-or-later.

`LICENSE.upstream.md` is upstream's copy of the GPL text, kept alongside the
files it covers. Pyren's own licence is at the repository root
(`LICENSE`), and `NOTICE` there records this credit too.

## The rule for this directory

**Nothing in Pyren writes here.** The installer copies this tree to
`/usr/src/hp-wmi-omen-1.0/` first and patches *that*, so this stays a
pristine snapshot and a second install never starts from the first one's
output. `daemon/crates/installer/src/detect.rs` finds it via
`REPO_DRIVER_DIR` for a development build; a package must install it to
`/usr/share/pyren/driver` (or point `PYREN_DRIVER_DIR` at a copy).

Updating it means replacing the files wholesale from a newer upstream, then
running `cargo test -p pyren-installer` — the patcher's tests check every
anchor it relies on against the real file here, so a table renamed upstream
fails a test rather than an install.
