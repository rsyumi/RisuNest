import { readFileSync } from 'node:fs'
import { zipSync } from 'fflate'
import { beforeEach, describe, expect, it, vi } from 'vitest'

// The importer's converters, without the app state their module loads.
vi.mock('../characterCards', () => ({
    convertCharbook: ({ charbook }: any) => ({
        lorebook: charbook.entries.map((entry: any) => ({ comment: entry.comment, content: entry.content })),
        loresettings: { scanDepth: 1 },
        loreExt: charbook.extensions,
    }),
    convertOffSpecCards: (card: any) => ({ name: card.name, desc: card.description, firstMessage: card.first_mes, image: undefined }),
}))
vi.mock('../characters', () => ({ createBlankChar: vi.fn() }))
vi.mock('../storage/database.svelte', () => ({ defaultSdDataFunc: () => [] }))
vi.mock('../globalApi.svelte', async () => ({ ...await import('../appendableBuffer'), VirtualWriter: class {} }))
vi.mock('../util', () => ({ blobToUint8Array: vi.fn() }))
const core = vi.hoisted(() => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: core.invoke }))

import { encodeRPack } from '../rpack/rpack_js'
import { readUpdateBaseFile, UnsupportedUpdateBaseFileError } from './baseFile'

beforeEach(() => {
    const map = readFileSync('src/ts/rpack/rpack_map.bin')
    vi.stubGlobal('fetch', vi.fn(async () => ({
        arrayBuffer: async () => map.buffer.slice(map.byteOffset, map.byteOffset + map.byteLength),
    })))
})

const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value))

const cardV3 = {
    spec: 'chara_card_v3',
    spec_version: '3.0',
    data: {
        name: 'Luna',
        description: 'desc',
        first_mes: 'hello',
        post_history_instructions: 'post',
        character_version: '1.0',
        nickname: 'Lu',
        extensions: {
            color: 'red', depth_prompt: { depth: 1 },
            risuai: { customScripts: [{ comment: 'r', in: 'a', out: 'b', type: 'editoutput' }], onboarding: { enabled: true, title: 'Welcome' }, source: ['from-risuai'] },
        },
        character_book: { extensions: {}, entries: [{ keys: ['town'], content: 'town lore', extensions: {}, enabled: true, insertion_order: 1, comment: 'Town' }] },
    },
}

async function risum(module: object): Promise<Uint8Array> {
    const main = await encodeRPack(encode({ type: 'risuModule', module }))
    const header = new Uint8Array(6)
    header[0] = 111
    new DataView(header.buffer).setUint32(2, main.length, true)
    const out = new Uint8Array(6 + main.length)
    out.set(header)
    out.set(main, 6)
    return out
}

function png(chunks: Record<string, string>): Uint8Array {
    const parts = [Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10])]
    const chunk = (type: string, body: Uint8Array) => {
        const out = new Uint8Array(12 + body.length)
        new DataView(out.buffer).setUint32(0, body.length)
        out.set(new TextEncoder().encode(type), 4)
        out.set(body, 8)
        return out
    }
    for (const [key, value] of Object.entries(chunks)) parts.push(chunk('tEXt', new TextEncoder().encode(`${key}\0${value}`)))
    parts.push(chunk('IEND', new Uint8Array()))
    const out = new Uint8Array(parts.reduce((size, part) => size + part.length, 0))
    let at = 0
    for (const part of parts) {
        out.set(part, at)
        at += part.length
    }
    return out
}

const base64 = (value: unknown) => Buffer.from(JSON.stringify(value)).toString('base64')

describe('readUpdateBaseFile', () => {
    it('reads the V3 card of a PNG before its V2 card', async () => {
        const v2 = { ...cardV3, spec: 'chara_card_v2', data: { ...cardV3.data, name: 'Old' } }
        const base = await readUpdateBaseFile('character', { name: 'Luna.PNG', data: png({ chara: base64(v2), ccv3: base64(cardV3) }) })
        expect(base).toMatchObject({ name: 'Luna', nickname: 'Lu' })
        await expect(readUpdateBaseFile('character', { name: 'x.png', data: png({ chara: 'rcc||locked' }) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
    })

    it('reads the text fields of a character card as the importer maps them', async () => {
        const base = await readUpdateBaseFile('character', { name: 'Luna.json', data: encode(cardV3) }) as any
        expect(base).toMatchObject({
            name: 'Luna', desc: 'desc', firstMessage: 'hello', replaceGlobalNote: 'post', characterVersion: '1.0', nickname: 'Lu',
            additionalData: { character_version: '1.0' }, extentions: { color: 'red' }, customscript: [{ comment: 'r' }], onboarding: { enabled: true, title: 'Welcome' },
        })
        const v2 = await readUpdateBaseFile('character', { name: 'Luna.json', data: encode({ ...cardV3, spec: 'chara_card_v2' }) }) as any
        expect(v2.source).toEqual(['from-risuai'])
        expect(base.globalLore).toEqual([{ comment: 'Town', content: 'town lore' }])
        expect(base.loreSettings).toEqual({ scanDepth: 1 })
        expect(base).not.toHaveProperty('chaId')
        expect(base).not.toHaveProperty('image')
    })

    it('takes the lorebook and scripts of a CHARX from its module', async () => {
        const module = { name: 'Luna', description: '', id: 'm', lorebook: [{ key: 'k', comment: 'From module', content: 'c', mode: 'normal', insertorder: 1, alwaysActive: false, secondkey: '', selective: false }], regex: [], trigger: [] }
        const data = zipSync({ 'card.json': encode(cardV3), 'module.risum': await risum(module) })
        const base = await readUpdateBaseFile('character', { name: 'Luna.charx', data }) as any
        expect(base.globalLore.map((entry: any) => entry.comment)).toEqual(['From module'])
        expect(base.customscript).toEqual([])
        // As a module, a CHARX imports through its card, as the module importer converts it.
        const asModule = await readUpdateBaseFile('module', { name: 'Luna.charx', data: zipSync({ 'card.json': encode(cardV3), 'module.risum': await risum({ ...module, name: 'Embedded' }) }) }) as any
        expect(asModule.name).toBe('Luna')
        expect(asModule.lorebook.map((entry: any) => entry.comment)).toContain('From Character Description')
    })

    it('reads a picked file natively by its path to the same base as reading it whole', async () => {
        const module = { name: 'Luna', description: '', id: 'm', lorebook: [{ key: 'k', comment: 'From module', content: 'c', mode: 'normal', insertorder: 1, alwaysActive: false, secondkey: '', selective: false }], regex: [], trigger: [] }
        // The native reply: a tag, a little-endian length and the bytes of each part.
        const reply = (...parts: [number, Uint8Array][]) => {
            const out = new Uint8Array(parts.reduce((size, [, bytes]) => size + 5 + bytes.length, 0))
            let at = 0
            for (const [tag, bytes] of parts) {
                out[at] = tag
                new DataView(out.buffer).setUint32(at + 1, bytes.length, true)
                out.set(bytes, at + 5)
                at += 5 + bytes.length
            }
            return out.buffer
        }
        core.invoke.mockResolvedValueOnce(reply([2, encode(cardV3)], [3, encode({ type: 'risuModule', module })]))
        const charx = zipSync({ 'card.json': encode(cardV3), 'module.risum': await risum(module) })
        expect(await readUpdateBaseFile('character', { name: 'Luna.charx', path: '/picked/source' }))
            .toEqual(await readUpdateBaseFile('character', { name: 'Luna.charx', data: charx }))
        expect(core.invoke).toHaveBeenCalledWith('native_content_update_base_read', { path: '/picked/source', format: 'charx' })
        core.invoke.mockResolvedValueOnce(reply([5, new TextEncoder().encode(base64(cardV3))]))
        expect(await readUpdateBaseFile('character', { name: 'Luna.png', path: '/picked/Luna.png' }))
            .toEqual(await readUpdateBaseFile('character', { name: 'Luna.png', data: png({ chara: base64(cardV3) }) }))
        // A file the native reader finds no module in, and a reply cut short.
        core.invoke.mockRejectedValueOnce({ code: 'unsupported-format', message: 'not a Risu module' })
        await expect(readUpdateBaseFile('module', { name: 'Lore.risum', path: '/picked/Lore.risum' })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        core.invoke.mockResolvedValueOnce(reply([2, encode(cardV3)]).slice(0, 9))
        await expect(readUpdateBaseFile('character', { name: 'Luna.charx', path: '/picked/source' })).rejects.toThrow('invalid update base')
    })

    it('reads the CHARX a JPEG carries after image bytes that look like a ZIP header', async () => {
        const charx = zipSync({ 'card.json': encode(cardV3) })
        const image = Uint8Array.from([0xff, 0xd8, 0x50, 0x4b, 3, 4, 0, 0, 0, 0, 0xff, 0xd9])
        const data = new Uint8Array(image.length + charx.length)
        data.set(image)
        data.set(charx, image.length)
        expect(await readUpdateBaseFile('character', { name: 'Luna.jpg', data })).toMatchObject({ name: 'Luna' })
    })

    it('refuses a CHARX whose card is larger than the importer takes, without inflating it', async () => {
        const data = zipSync({ 'card.json': encode(cardV3) })
        // The central directory entry names the size the card inflates to.
        const entry = data.findIndex((_, at) => data[at] === 0x50 && data[at + 1] === 0x4b && data[at + 2] === 1 && data[at + 3] === 2)
        new DataView(data.buffer, data.byteOffset).setUint32(entry + 24, 0xffffffff, true)
        await expect(readUpdateBaseFile('character', { name: 'Luna.charx', data })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
    })

    it('reads a module file without the install bookkeeping of another install', async () => {
        const module = { name: 'Lore', description: 'd', id: 'other-install', updateLineage: 'file', updateBase: 'assets/x.json', assets: [['a', 'assets/a.png', 'png']], lorebook: [] }
        expect(await readUpdateBaseFile('module', { name: 'Lore.risum', data: await risum(module) })).toEqual({ name: 'Lore', description: 'd', lorebook: [] })
        expect(await readUpdateBaseFile('module', { name: 'Lore.json', data: encode({ ...module, type: 'risuModule' }) })).toEqual({ name: 'Lore', description: 'd', lorebook: [] })
    })

    it('reads a lorebook or regex file as the module its import converts it to', async () => {
        const lore = { key: 'k', comment: 'Town', content: 'c' }
        expect(await readUpdateBaseFile('module', { name: 'Lore.lorebook', data: encode({ type: 'risu', name: 'Lore', data: [lore] }) }))
            .toEqual({ name: 'Lore', description: 'Converted from risu lorebook', lorebook: [lore] })
        expect(await readUpdateBaseFile('module', { name: 'Regex.json', data: encode({ type: 'regex', data: [] }) }))
            .toEqual({ name: 'Imported Regex', description: 'Converted from risu regex', regex: [] })
    })

    it('reads an older card the importer takes through its old-format reader', async () => {
        const old = { name: 'Luna', description: 'd', first_mes: 'hi' }
        const read = { name: 'Luna', desc: 'd', firstMessage: 'hi' }
        expect(await readUpdateBaseFile('character', { name: 'Luna.json', data: encode(old) })).toEqual(read)
        // The PNG importer takes one without a description too.
        expect(await readUpdateBaseFile('character', { name: 'Luna.png', data: png({ chara: base64({ ...old, description: '' }) }) })).toEqual({ ...read, desc: '' })
    })

    it('refuses files that hold no card or module', async () => {
        await expect(readUpdateBaseFile('character', { name: 'Lore.risum', data: await risum({ name: 'Lore' }) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        await expect(readUpdateBaseFile('character', { name: 'notes.txt', data: encode('x') })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        await expect(readUpdateBaseFile('character', { name: 'old.json', data: encode({ spec: 'other' }) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        await expect(readUpdateBaseFile('character', { name: 'empty.charx', data: zipSync({ 'card.json': encode({}) }) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        await expect(readUpdateBaseFile('module', { name: 'empty.json', data: encode(null) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        await expect(readUpdateBaseFile('module', { name: 'text.risum', data: await risum('Lore' as unknown as object) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        // A card without the metadata the importers need.
        for (const data of [null, {}, { extensions: { risuai: [] } }]) {
            await expect(readUpdateBaseFile('character', { name: 'bad.charx', data: zipSync({ 'card.json': encode({ spec: 'chara_card_v3', data }) }) })).rejects.toBeInstanceOf(UnsupportedUpdateBaseFileError)
        }
    })
})
