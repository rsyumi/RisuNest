import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import startup from "../../../src-tauri/src/windows_appearance/startup.js?raw";
import {
  opaqueHex,
  resolveAppearance,
  WINDOWS_APPEARANCE_CACHE,
} from "./windowsAppearance";

vi.mock("../platform", () => ({ isTauriDesktop: false }));

function rgba(value: string): [number, number, number, number] | undefined {
  if (!/^#[a-f0-9]{6}$/i.test(value)) return undefined;
  return [
    parseInt(value.slice(1, 3), 16),
    parseInt(value.slice(3, 5), 16),
    parseInt(value.slice(5, 7), 16),
    255,
  ];
}

describe("Windows native palette", () => {
  const palette = {
    bgcolor: "#301040",
    darkbg: "#200830",
    textcolor: "#eeddee",
    type: "dark" as const,
  };

  it("keeps a user palette instead of substituting a Fluent palette", () => {
    expect(resolveAppearance(palette, () => "", rgba)).toEqual({
      background: "#301040",
      caption: "#200830",
      text: "#eeddee",
      dark: true,
    });
  });

  it("uses resolved CSS overrides and falls back for non-color CSS", () => {
    const variables = {
      "--risu-theme-darkbg": " #776655 ",
      "--risu-theme-bgcolor": "url(image)",
      "--risu-theme-textcolor": "#abcdef",
    };
    expect(resolveAppearance(palette, (name) => variables[name], rgba)).toEqual(
      {
        background: "#301040",
        caption: "#776655",
        text: "#abcdef",
        dark: true,
      },
    );
  });

  it("composites transparent colors against the chosen surface", () => {
    expect(opaqueHex([255, 0, 0, 128], [0, 0, 255, 255])).toBe("#80007f");
    expect(opaqueHex([255, 255, 255, 0], [48, 16, 64, 255])).toBe("#301040");
  });

  it("uses the app's light preference independently of the OS", () => {
    expect(
      resolveAppearance({ ...palette, type: "light" }, () => "", rgba).dark,
    ).toBe(false);
  });
});

describe("Windows startup color cache", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("style");
  });
  afterEach(() => vi.restoreAllMocks());

  it("applies cached user colors before loading any app or database module", () => {
    localStorage.setItem(
      WINDOWS_APPEARANCE_CACHE,
      JSON.stringify({
        background: "#eefaff",
        caption: "#d0e8f0",
        text: "#102030",
        dark: false,
      }),
    );
    new Function(startup)();
    expect(
      document.documentElement.style.getPropertyValue("--risu-theme-darkbg"),
    ).toBe("#d0e8f0");
    expect(document.documentElement.style.colorScheme).toBe("light");
  });

  it("leaves defaults intact on first run", () => {
    new Function(startup)();
    expect(document.documentElement.getAttribute("style")).toBeNull();
  });

  it.each([
    "null",
    "{",
    '{"dark":true,"caption":"red"}',
    JSON.stringify({
      background: "#000000",
      caption: "url(secret)",
      text: "#ffffff",
      dark: true,
    }),
  ])("rejects malformed cache without echoing its content", (raw) => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => {});
    localStorage.setItem(WINDOWS_APPEARANCE_CACHE, raw);
    new Function(startup)();
    expect(document.documentElement.getAttribute("style")).toBeNull();
    expect(warning).toHaveBeenCalledExactlyOnceWith(
      "Could not read the Windows startup colors",
    );
  });
});
