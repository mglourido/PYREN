# Keyboard: Fn + P does not reach Linux

*2026-09-11 — measured on the OMEN test laptop under Hyprland.*

## The symptom

Pressing **Fn + P** — the key that pops the OMEN Gaming Hub widget on
Windows — does nothing on Linux, and produces no sound or visible feedback.

## Why

**Fn is not a key the OS can see.** It is handled in the keyboard
controller: it changes what the *other* keys in the combination report, and
never emits a scancode of its own. Nothing in userspace — no compositor, no
`evdev` reader — can bind "Fn".

The usual way around that is to bind the event the combination *does*
produce: many laptop function-row combos send an ordinary key or a vendor
hotkey that the kernel turns into a keycode on some `/dev/input/event*`
device, and a compositor like Hyprland binds *that*, not the physical key.

**On this laptop, Fn + P sends nothing at all.** Watched across every input
device, no event appears when the combination is pressed — not a keycode,
not an unmapped scancode, not an `HP WMI hotkeys` event. The firmware
consumes the combination whole and keeps it. There is nothing for the
kernel, Hyprland, or the daemon to hear.

## What this means for the feature

"Open the Pyren widget on Fn + P" **cannot be implemented** — there is no
signal to trigger on. This is not fixable in software on a machine that
behaves this way.

## What Pyren does instead

The `hotkey` daemon module never assumes a keycode. It asks the machine:
`hotkey.learn` listens for a few seconds, the user presses whatever
shortcut they want (a plain combination like `Ctrl+Alt+P` is the normal
choice, not a fallback), and whatever actually arrives is bound.

It is heard by the daemon through `/dev/input/event*` rather than by a
desktop keybinding, so the one shortcut works on every compositor and at
the login screen. If the user's chosen combination *does* reach Linux, the
widget opens on it; Fn + P itself never will on this hardware.
