import { pluginDeviceStorage as pluginStorage, pluginDevicePrefix } from "./pluginDeviceStorage";

// Structured so an owner or key holding the separator cannot reach another
// plugin's keyspace.
function deviceKey(owner: string, space: 'string' | 'json', key: string): string {
    return `${pluginDevicePrefix}${JSON.stringify([owner, space, key])}`;
}

function deviceKeyOwnerSpace(
    stored: string,
    owner: string,
    space: 'string' | 'json',
): string | null {
    if (!stored.startsWith(pluginDevicePrefix)) return null;
    try {
        const parsed = JSON.parse(stored.substring(pluginDevicePrefix.length));
        if (!Array.isArray(parsed) || parsed.length !== 3) return null;
        const [storedOwner, storedSpace, key] = parsed as unknown[];
        if (storedOwner !== owner || storedSpace !== space) return null;
        return typeof key === 'string' ? key : null;
    } catch {
        return null;
    }
}

export class SafeLocalStorage {
    readonly #owner: string;

    constructor(owner: string) {
        this.#owner = owner;
    }

    getItem(key: string): string | null {
        return localStorage.getItem(deviceKey(this.#owner, 'string', key));
    }
    setItem(key: string, value: string): void {
        localStorage.setItem(deviceKey(this.#owner, 'string', key), value);
    }
    removeItem(key: string): void {
        localStorage.removeItem(deviceKey(this.#owner, 'string', key));
    }
    //not a standard localStorage method, but useful
    keys(): string[] {
        const keys: string[] = [];
        for (let i = 0; i < localStorage.length; i++) {
            const stored = localStorage.key(i);
            const key = stored === null
                ? null
                : deviceKeyOwnerSpace(stored, this.#owner, 'string');
            if (key !== null) keys.push(key);
        }
        return keys;
    }

    key(index: number): string | null {
        const safeKeys = this.keys();
        return safeKeys[index] || null;
    }

    clear(): void {
        const keys = this.keys();
        for (const key of keys) {
            this.removeItem(key);
        }
    }

    get length(): number {
        return this.keys().length;
    }


}


export class SafeLocalPluginStorage {
    __classType = 'REMOTE_REQUIRED' as const;
    readonly #owner: string;

    constructor(owner: string) {
        this.#owner = owner;
    }

    async getItem<T>(key: string): Promise<T | null> {
        return await pluginStorage.getItem<T>(deviceKey(this.#owner, 'json', key));
    }
    async setItem<T>(key: string, value: T): Promise<void> {
        await pluginStorage.setItem(deviceKey(this.#owner, 'json', key), value);
    }
    async removeItem(key: string): Promise<void> {
        await pluginStorage.removeItem(deviceKey(this.#owner, 'json', key));
    }
    async keys(): Promise<string[]> {
        const keys: string[] = [];
        const owner = this.#owner;
        await pluginStorage.iterate((value, stored) => {
            const key = deviceKeyOwnerSpace(stored, owner, 'json');
            if (key !== null) keys.push(key);
        });
        return keys;
    }
    async clear(): Promise<void> {
        const keys = await this.keys();
        for (const key of keys) {
            await this.removeItem(key);
        }
    }
}

export const tagWhitelist = [
    'a',
    'abbr',
    'acronym',
    'address',
    'area',
    'article',
    'aside',
    'audio',
    'b',
    'bdi',
    'bdo',
    'big',
    'blink',
    'blockquote',
    'body',
    'br',
    'button',
    'canvas',
    'caption',
    'center',
    'cite',
    'code',
    'col',
    'colgroup',
    'content',
    'data',
    'datalist',
    'dd',
    'decorator',
    'del',
    'details',
    'dfn',
    'dialog',
    'dir',
    'div',
    'dl',
    'dt',
    'element',
    'em',
    'fieldset',
    'figcaption',
    'figure',
    'font',
    'footer',
    'form',
    'h1',
    'h2',
    'h3',
    'h4',
    'h5',
    'h6',
    'head',
    'header',
    'hgroup',
    'hr',
    'html',
    'i',
    'img',
    'input',
    'ins',
    'kbd',
    'label',
    'legend',
    'li',
    'main',
    'map',
    'mark',
    'marquee',
    'menu',
    'menuitem',
    'meter',
    'nav',
    'nobr',
    'ol',
    'optgroup',
    'option',
    'output',
    'p',
    'picture',
    'pre',
    'progress',
    'q',
    'rp',
    'rt',
    'ruby',
    's',
    'samp',
    'search',
    'section',
    'select',
    'shadow',
    'slot',
    'small',
    'source',
    'spacer',
    'span',
    'strike',
    'strong',
    'style',
    'sub',
    'summary',
    'sup',
    'table',
    'tbody',
    'td',
    'template',
    'textarea',
    'tfoot',
    'th',
    'thead',
    'time',
    'tr',
    'track',
    'tt',
    'u',
    'ul',
    'var',
    'video',
    'wbr',
    'svg',
    'a',
    'altglyph',
    'altglyphdef',
    'altglyphitem',
    'animatecolor',
    'animatemotion',
    'animatetransform',
    'circle',
    'clippath',
    'defs',
    'desc',
    'ellipse',
    'enterkeyhint',
    'exportparts',
    'filter',
    'font',
    'g',
    'glyph',
    'glyphref',
    'hkern',
    'image',
    'inputmode',
    'line',
    'lineargradient',
    'marker',
    'mask',
    'metadata',
    'mpath',
    'part',
    'path',
    'pattern',
    'polygon',
    'polyline',
    'radialgradient',
    'rect',
    'stop',
    'style',
    'switch',
    'symbol',
    'text',
    'textpath',
    'title',
    'tref',
    'tspan',
    'view',
    'vkern',
];
