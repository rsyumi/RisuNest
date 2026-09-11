import { expect, it, vi } from "vitest";
import { flushSync, mount, tick, unmount } from "svelte";
import AssetInput from "./AssetInput.svelte";
import type { character } from "src/ts/storage/database.svelte";
import { getFileSrc } from "src/ts/globalApi.svelte";

vi.mock("src/ts/globalApi.svelte", () => ({
  getFileSrc: vi.fn(async (path: string) => `/synthetic/${path}`),
  saveAsset: vi.fn(),
}));
vi.mock("src/ts/util", () => ({ selectMultipleFile: vi.fn() }));
vi.mock("@lucide/svelte", () => ({
  FileMusicIcon: () => {},
  PlusIcon: () => {},
  ChevronLeftIcon: () => {},
  ChevronRightIcon: () => {},
}));

it("requests only visible previews and selects the correct backing asset after paging a 10,000 asset library", async () => {
  const assets = Array.from(
    { length: 10_000 },
    (_, i) => [`asset ${i}`, `${i}.png`, "png"] as [string, string, string],
  );
  const onSelect = vi.fn();
  const target = document.createElement("div");
  document.body.append(target);
  const component = mount(AssetInput, {
    target,
    props: {
      currentCharacter: {
        type: "character",
        additionalAssets: assets,
      } as character,
      onSelect,
    },
  });
  try {
    flushSync();
    await tick();
    expect(getFileSrc).toHaveBeenCalledTimes(60);
    expect(target.querySelectorAll("img")).toHaveLength(60);
    (
      target.querySelector('[aria-label="Next page"]') as HTMLButtonElement
    ).click();
    flushSync();
    await tick();
    expect(getFileSrc).toHaveBeenCalledTimes(120);
    expect(target.querySelectorAll("img")).toHaveLength(60);
    (
      target.querySelector('img[alt="asset 60"]')!
        .parentElement as HTMLButtonElement
    ).click();
    expect(onSelect).toHaveBeenCalledExactlyOnceWith(assets[60]);
    (
      target.querySelector('[aria-label="Previous page"]') as HTMLButtonElement
    ).click();
    flushSync();
    await tick();
    expect(target.querySelector('img[alt="asset 0"]')).not.toBeNull();
    expect(target.querySelectorAll("img")).toHaveLength(60);
  } finally {
    await unmount(component);
    target.remove();
  }
});
