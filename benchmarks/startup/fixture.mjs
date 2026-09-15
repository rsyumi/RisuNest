import { DatabaseSync } from 'node:sqlite'
import { mkdir, writeFile } from 'node:fs/promises'
import { createHash } from 'node:crypto'
import { deflateSync } from 'node:zlib'
import { crc32 } from 'crc'
import path from 'node:path'

export function assertSyntheticProfile(root, identifier) {
    if (
        !/^RisuNest\.phase3benchmark\.r[0-9a-f]{12}\.[a-zA-Z0-9]+$/.test(identifier) ||
        path.basename(path.resolve(root)) !== identifier
    ) {
        throw new Error('Refusing non-benchmark profile')
    }
}

export function seedExpression({
    characters = 11,
    bytes = 98_000_000,
    compatibility = true,
    images = false,
    mutation = false,
    investigation = false,
    referencedAssets = 0,
    coldStorage = false,
    pluginBytes = 46_000_000,
    locale = 'en',
}) {
    return `(async () => {
        let step = 0;
        try {
        const invoke = window.__TAURI_INTERNALS__.invoke;
        const source = await invoke('pds_materialize');
        localStorage.setItem('tos4', 'true');
        step = 1;
        const {characters: unused, botPresets, ...root} = source;
        root.characterOrder = Array.from({length: ${characters}}, (_, i) => 'synthetic-' + i);
        root.coldstorage = ${coldStorage}; root.modules = []; root.didFirstSetup = true;
        root.plugins = [{name: 'Synthetic no-op', version: '2.1', enabled: ${compatibility},
            script: ${JSON.stringify(mutation ? 'const db = getDatabase(); db.temperature = db.temperature === 0.81 ? 0.82 : 0.81;' : '// Synthetic no-op')}, arg: {}, realArg: {}}];
        root.language = ${JSON.stringify(locale)};
        root.pluginCustomStorage = {synthetic: 'x'.repeat(${investigation ? 46_756_003 : pluginBytes})};
        root.syntheticRootPadding = 'x'.repeat(${investigation ? 6_500_000 : 0});
        const shape = ${
            investigation
                ? JSON.stringify([
                      [8, 60, 362477, 318135, 22220],
                      [4, 126, 1049208, 703415, 120603],
                      [9, 30, 81090, 61127, 1378],
                      [5, 39, 512334, 208057, 204926],
                      [3, 128, 1970244, 1835017, 57812],
                      [1, 0, 19402, 0, 19111],
                      [73, 1697, 34946015, 24228751, 38614],
                      [10, 64, 1156803, 722452, 227243],
                      [5, 188, 2710318, 1787008, 573055],
                      [2, 96, 1063635, 770887, 179397],
                      [8, 50, 923419, 415100, 236369],
                  ])
                : 'null'
        };
        const {stagingId} = await invoke('pds_replace_begin');
        step = 2;
        const count = ${characters};
        const bodyBytes = Math.max(0, ${bytes} - JSON.stringify(root).length);
        let measuredBytes = JSON.stringify(root).length + JSON.stringify(botPresets).length;
        let conversationCount = 0, messageCount = 0;
        try {
            await invoke('pds_replace_put_root', {stagingId, root});
            step = 3;
            await invoke('pds_replace_put_presets', {stagingId, presets: botPresets});
            step = 4;
            for (let i = 0; i < count; i++) {
                const [conversations, messages, jsonBytes, textBytes, metadataBytes] = shape?.[i]
                    ?? [4, 32, Math.floor(bodyBytes / count), Math.floor(bodyBytes / count * 0.7), 200];
                const character = {type: 'character', chaId: 'synthetic-' + i, name: 'Synthetic ' + i,
                    chatPage: 0, firstMessage: 'Synthetic greeting', desc: 'x'.repeat(metadataBytes), chats: [],
                    notes: '', chatFolders: [], emotionImages: [], bias: [], viewScreen: 'none', globalLore: [],
                    sdData: [], utilityBot: false, customscript: [], triggerscript: [], exampleMessage: '',
                    creatorNotes: '', systemPrompt: '', postHistoryInstructions: '', replaceGlobalNote: '',
                    alternateGreetings: [], tags: [], creator: '', characterVersion: '', personality: '',
                    scenario: '', firstMsgIndex: -1, additionalText: ''};
                if (${images}) character.image = 'assets/synthetic-' + (i % 16) + '.png';
                if (${referencedAssets} > 0) character.additionalAssets = Array.from(
                    {length: Math.max(0, Math.ceil((${referencedAssets} - i) / count))}, (_, j) => {
                        const asset = i + j * count;
                        return ['Synthetic ' + asset, 'assets/synthetic-' + asset + (${images} && asset < 16 ? '.png' : '.bin'), 'bin'];
                    });
                let left = messages, messageIndex = 0;
                for (let j = 0; j < conversations; j++) {
                    const n = Math.ceil(left / (conversations - j)); left -= n;
                    character.chats.push({id: 'synthetic-chat-' + i + '-' + j, name: 'Synthetic chat', note: '', localLore: [],
                        message: Array.from({length: n}, () => ({role: 'user', data: 'x'.repeat(messages ? Math.floor(textBytes / messages) : 0),
                            chatId: 'synthetic-msg-' + i + '-' + messageIndex++}))});
                }
                const size = JSON.stringify(character).length;
                if (size < jsonBytes) character.syntheticPadding = 'x'.repeat(jsonBytes - size);
                measuredBytes += JSON.stringify(character).length;
                conversationCount += character.chats.length; messageCount += messages;
                await invoke('pds_replace_add_characters', {stagingId, characters: [character]});
            }
            const {revision} = await invoke('pds_replace_preserve_repositories', {stagingId});
            step = 5;
            await invoke('pds_replace_commit', {stagingId, expectedRevision: revision});
            return {success: true, characters: count, targetBytes: ${bytes}, measuredBytes, conversationCount, messageCount};
        } catch (error) { await invoke('pds_replace_abort', {stagingId}); throw error; }
        } catch (error) {
            const text = typeof error === 'string' ? error : String(error?.message ?? '');
            return {success: false, step,
                validation: error?.code === 'validation', conflict: error?.code === 'revision-conflict',
                storeError: error?.code === 'store-error',
                undefinedValue: text.includes('undefined'), missing: text.includes('missing'),
                tooLarge: /large|limit|size/i.test(text),
                identifier: /id|ID/.test(text), generation: /generation|staging/.test(text),
            };
        }
    })()`
}

// The app must be stopped before calling this offline fixture writer.
export async function addSyntheticAssets(root, identifier, count, images = false) {
    assertSyntheticProfile(root, identifier)
    const db = new DatabaseSync(path.join(root, 'persistent/persistent.sqlite'))
    try {
        const active = JSON.parse(
            db.prepare("SELECT value FROM meta WHERE key='activeGeneration'").get().value,
        )
        const objectInsert = db.prepare(
            'INSERT INTO asset_objects (object_hash,byte_size,created_at_ms) VALUES (?,?,?)',
        )
        const aliasInsert = db.prepare(
            "INSERT INTO asset_aliases (generation,logical_key,object_hash,kind,size,mime,name,ext) VALUES (?,?,?,'asset',?,?,?,?)",
        )
        db.exec('BEGIN')
        let next = db.prepare('SELECT COUNT(*) AS count FROM asset_objects').get().count
        if (next > count) throw new Error('Cannot shrink the synthetic catalog in place')
        let totalBytes = db
            .prepare('SELECT COALESCE(SUM(byte_size),0) AS bytes FROM asset_objects')
            .get().bytes
        await Promise.all(
            Array.from({ length: 16 }, async () => {
                while (next < count) {
                    const index = next++
                    const isImage = images && index < 16
                    const payload = isImage
                        ? syntheticPng(index)
                        : Buffer.from('Synthetic startup asset ' + index)
                    const hash = createHash('sha256').update(payload).digest('hex')
                    const directory = path.join(root, 'assets-v2/objects', hash.slice(0, 2))
                    await mkdir(directory, { recursive: true })
                    await writeFile(path.join(directory, hash.slice(2)), payload, { flag: 'wx' })
                    objectInsert.run(hash, payload.length, 0)
                    aliasInsert.run(
                        active,
                        `assets/synthetic-${index}.${isImage ? 'png' : 'bin'}`,
                        hash,
                        payload.length,
                        isImage ? 'image/png' : 'application/octet-stream',
                        'Synthetic',
                        isImage ? 'png' : 'bin',
                    )
                    totalBytes += payload.length
                }
            }),
        )
        db.exec('COMMIT')
        const aliases = db
            .prepare('SELECT COUNT(*) AS count FROM asset_aliases WHERE generation=?')
            .get(active).count
        const objects = db.prepare('SELECT COUNT(*) AS count FROM asset_objects').get().count
        if (aliases !== count || objects !== count)
            throw new Error('Synthetic catalog counts do not match the workload')
        return {
            objects,
            aliases,
            payloadBytes: totalBytes,
            imageObjects: images ? Math.min(16, count) : 0,
        }
    } finally {
        db.close()
    }
}

export function syntheticPng(index) {
    const chunk = (type, payload) => {
        const name = Buffer.from(type)
        const length = Buffer.alloc(4)
        length.writeUInt32BE(payload.length)
        const checksum = Buffer.alloc(4)
        checksum.writeUInt32BE(crc32(Buffer.concat([name, payload])))
        return Buffer.concat([length, name, payload, checksum])
    }
    const header = Buffer.alloc(13)
    header.writeUInt32BE(256)
    header.writeUInt32BE(256, 4)
    header[8] = 8
    header[9] = 6
    const pixels = Buffer.alloc((256 * 4 + 1) * 256)
    for (let y = 0; y < 256; y++)
        for (let x = 0; x < 256; x++) {
            const offset = y * 1025 + 1 + x * 4
            pixels[offset] = (x + index * 11) % 256
            pixels[offset + 1] = (y + index * 7) % 256
            pixels[offset + 2] = 128
            pixels[offset + 3] = 255
        }
    return Buffer.concat([
        Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
        chunk('IHDR', header),
        chunk('IDAT', deflateSync(pixels)),
        chunk('IEND', Buffer.alloc(0)),
    ])
}
