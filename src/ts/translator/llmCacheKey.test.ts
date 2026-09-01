import { describe, expect, it, vi } from "vitest";

vi.mock("../storage/database.svelte", () => ({
    getDatabase: () => ({ presetRegex: [] }),
}));

vi.mock("../process/modules", () => ({
    getModuleRegexScripts: () => [],
    moduleUpdate: () => {},
}));

vi.mock("localforage", () => {
    const store = new Map<string, string>();
    return {
        default: {
            createInstance: () => ({
                getItem: async (key: string) => (store.has(key) ? store.get(key)! : null),
                setItem: async (key: string, value: string) => { store.set(key, value); },
                iterate: async () => undefined,
                clear: async () => { store.clear(); },
            }),
        },
    };
});

import { getLLMCache, setLLMCache } from "./translator";

describe("LLM cache key normalization", () => {
    it("matches keys that differ only by deferred inlay slot markers", async () => {
        const withSlot = '<p>hi</p><img data-risu-inlay-id="a" data-risu-inlay-slot="0" loading="lazy"/>';
        const differentSlot = withSlot.replace('data-risu-inlay-slot="0"', 'data-risu-inlay-slot="1a"');
        const withoutSlot = withSlot.replace(' data-risu-inlay-slot="0"', '');

        await setLLMCache(withSlot, "translated");

        expect(await getLLMCache(withSlot)).toBe("translated");
        expect(await getLLMCache(differentSlot)).toBe("translated");
        expect(await getLLMCache(withoutSlot)).toBe("translated");
    });

    it("keeps keys with different content distinct", async () => {
        await setLLMCache("<p>a</p>", "A");
        expect(await getLLMCache("<p>b</p>")).toBeNull();
    });
});
