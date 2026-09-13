import { invoke } from "@tauri-apps/api/core";
import { type as osType } from "@tauri-apps/plugin-os";
import { isTauriDesktop } from "../platform";

export const WINDOWS_APPEARANCE_CACHE = "risunest.windowsAppearance";

export interface WindowPalette {
  bgcolor: string;
  darkbg: string;
  textcolor: string;
  type: "light" | "dark";
}

export interface WindowsAppearance {
  background: string;
  caption: string;
  text: string;
  dark: boolean;
}

type Rgba = readonly [number, number, number, number];

/** Composite CSS alpha against the opaque surface DWM can actually display. */
export function opaqueHex(color: Rgba, background: Rgba): string {
  const alpha = color[3] / 255;
  return (
    "#" +
    color
      .slice(0, 3)
      .map((channel, index) =>
        Math.round(channel * alpha + background[index] * (1 - alpha))
          .toString(16)
          .padStart(2, "0"),
      )
      .join("")
  );
}

export function resolveAppearance(
  palette: WindowPalette,
  variable: (name: string) => string,
  rgba: (color: string) => Rgba | undefined,
): WindowsAppearance {
  const base: Rgba =
    palette.type === "light" ? [255, 255, 255, 255] : [40, 42, 54, 255];
  const resolve = (key: "bgcolor" | "darkbg" | "textcolor", fallback: Rgba) =>
    rgba(variable(`--risu-theme-${key}`).trim()) ??
    rgba(palette[key]) ??
    fallback;
  const background = opaqueHex(resolve("bgcolor", base), base);
  const backgroundRgba = rgba(background) ?? base;
  const caption = opaqueHex(resolve("darkbg", backgroundRgba), backgroundRgba);
  const text = opaqueHex(
    resolve(
      "textcolor",
      palette.type === "light" ? [0, 0, 0, 255] : [255, 255, 255, 255],
    ),
    rgba(caption) ?? backgroundRgba,
  );
  return { background, caption, text, dark: palette.type === "dark" };
}

function readAppearance(palette: WindowPalette): WindowsAppearance {
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (!context) throw new Error("Color conversion unavailable");
  const probe = document.createElement("span");
  probe.style.cssText = "position:fixed;visibility:hidden;pointer-events:none";
  document.documentElement.appendChild(probe);
  try {
    const styles = getComputedStyle(document.documentElement);
    return resolveAppearance(
      palette,
      (name) => styles.getPropertyValue(name),
      (color) => {
        if (!color || !CSS.supports("color", color)) return undefined;
        probe.style.color = color;
        const resolved = getComputedStyle(probe).color;
        if (!resolved || !CSS.supports("color", resolved)) return undefined;
        context.clearRect(0, 0, 1, 1);
        context.fillStyle = resolved;
        context.fillRect(0, 0, 1, 1);
        return Array.from(
          context.getImageData(0, 0, 1, 1).data,
        ) as unknown as Rgba;
      },
    );
  } finally {
    probe.remove();
  }
}

let pending: ReturnType<typeof setTimeout> | undefined;
let latest: WindowPalette | undefined;
let lastApplied = "";
let updates = Promise.resolve();

/** Run after both the palette and custom CSS have been applied in this turn. */
export function scheduleWindowsAppearance(palette?: WindowPalette): void {
  if (!isTauriDesktop || osType() !== "windows") return;
  if (palette) latest = { ...palette };
  if (!latest) return;
  clearTimeout(pending);
  pending = setTimeout(() => {
    let appearance: WindowsAppearance;
    try {
      appearance = readAppearance(latest);
      document.documentElement.style.colorScheme = appearance.dark
        ? "dark"
        : "light";
    } catch {
      console.warn("Could not resolve the Windows window colors");
      return;
    }
    const serialized = JSON.stringify(appearance);
    updates = updates.then(async () => {
      if (serialized === lastApplied) return;
      try {
        await invoke("windows_set_appearance", { appearance });
        lastApplied = serialized;
        try {
          localStorage.setItem(WINDOWS_APPEARANCE_CACHE, serialized);
        } catch {
          console.warn("Could not cache the Windows startup colors");
        }
      } catch {
        console.warn("Could not apply the Windows window colors");
      }
    });
  }, 0);
}
