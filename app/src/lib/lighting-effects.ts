/**
 * The lighting page's preview of an effect: the same frames the daemon
 * writes, drawn on the four-zone bar on screen.
 *
 * A port of `frame()` in `daemon/crates/rgb/src/effects.rs`, and only for
 * looking at - the daemon's copy is the one that reaches the keyboard, and
 * the one with tests. Kept formula for formula so the preview does not
 * show a different effect from the one on the keys; a change there wants
 * the same change here.
 */
import type { RgbEffect } from "$lib/api/daemon";

const ZONES = 4;
const SPEED_DEFAULT = 5;

const BASE_PERIOD: Record<RgbEffect["kind"], number> = {
  breathing: 4,
  spectrum: 8,
  rainbowWave: 4,
  wave: 2,
  fade: 3,
};

type Rgb = [number, number, number];

function parse(hex: string): Rgb {
  const h = hex.replace("#", "");
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16) || 0) as Rgb;
}

function toHex(c: Rgb): string {
  return "#" + c.map((v) => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, "0")).join("");
}

const mod = (x: number, m: number) => ((x % m) + m) % m;
const smooth = (x: number) => {
  const c = Math.max(0, Math.min(1, x));
  return c * c * (3 - 2 * c);
};
const dim = (c: Rgb, k: number): Rgb => [c[0] * k, c[1] * k, c[2] * k];
const mix = (a: Rgb, b: Rgb, k: number): Rgb => [0, 1, 2].map((i) => a[i] + (b[i] - a[i]) * k) as Rgb;

function hue(turns: number): Rgb {
  const h = mod(turns, 1) * 6;
  const x = 1 - Math.abs((h % 2) - 1);
  const [r, g, b] = [
    [1, x, 0],
    [x, 1, 0],
    [0, 1, x],
    [0, x, 1],
    [x, 0, 1],
    [1, 0, x],
  ][Math.min(5, Math.floor(h))];
  return [r * 255, g * 255, b * 255];
}

/** The four zones at `t` seconds into `effect`, as `#rrggbb`. */
export function frame(effect: RgbEffect, t: number): string[] {
  const period = (BASE_PERIOD[effect.kind] * SPEED_DEFAULT) / Math.max(1, effect.speed);
  const phase = mod(t / period, 1);
  const colors = effect.colors.map(parse);
  const color = (i: number): Rgb => (colors.length ? colors[i % colors.length] : [0, 0, 0]);
  const position = (z: number) => (effect.direction === "rightToLeft" ? ZONES - 1 - z : z);
  const zones = Array.from({ length: ZONES }, (_, z) => z);

  let out: Rgb[];
  switch (effect.kind) {
    case "breathing": {
      const level = (1 + Math.cos(phase * 2 * Math.PI)) / 2;
      out = zones.map((z) => dim(color(z), level));
      break;
    }
    case "spectrum":
      out = zones.map(() => hue(phase));
      break;
    case "rainbowWave":
      out = zones.map((z) => hue(phase - position(z) / ZONES));
      break;
    case "wave": {
      const margin = 0.75;
      const width = 0.5;
      const centre = phase * (ZONES - 1 + 2 * margin) - margin;
      const back = colors.length > 1 ? color(1) : ([0, 0, 0] as Rgb);
      out = zones.map((z) => {
        const d = position(z) - centre;
        return mix(back, color(0), Math.exp(-(d * d) / width));
      });
      break;
    }
    case "fade": {
      let c: Rgb;
      if (colors.length <= 1) {
        c = dim(color(0), (1 + Math.cos(phase * 2 * Math.PI)) / 2);
      } else {
        const along = mod(t / period, colors.length);
        const from = Math.floor(along);
        c = mix(color(from), color(from + 1), smooth(along - from));
      }
      out = zones.map(() => c);
      break;
    }
  }
  return out.map(toHex);
}
