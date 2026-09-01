# Icons

`source.svg` is the master artwork — an OMEN-style rotated diamond in the
brand gradient, drawn from scratch (no HP assets are used anywhere in this
project). Everything else here is generated from it.

To regenerate after editing the SVG, from `app/`:

```sh
rsvg-convert -w 1024 -h 1024 src-tauri/icons/source.svg -o /tmp/appicon.png
bun run tauri icon /tmp/appicon.png
```

The Tauri CLI also emits `android/` and `ios/` icon sets; they are deleted
on purpose, since this is a Linux desktop application.
