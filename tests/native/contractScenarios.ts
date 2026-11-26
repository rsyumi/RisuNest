import { RevisionConflictError, SnapshotReleasedError, type PersistentDataStore, type AssetAlias } from '../../src/ts/storage/persistentDataStore'
import type { Database, Message } from '../../src/ts/storage/database.svelte'
import { normalizeConversationContextInput, readPinnedConversationContext } from '../../src/ts/plugins/conversationContext'
import { normalizeConversationPatchInput, planStoredConversationPatch, prepareConversationPatchCommit } from '../../src/ts/plugins/conversationPatch'

const canonical = (value: unknown): string => JSON.stringify(value, (_key, item) =>
    item && typeof item === 'object' && !Array.isArray(item)
        ? Object.fromEntries(Object.keys(item).sort().map(key => [key, item[key]])) : item)
function equal(actual: unknown, expected: unknown, label: string) {
    if (canonical(actual) !== canonical(expected)) throw new Error(label)
}
const largeText = 'synthetic'.repeat(160_000)
const aliases: AssetAlias[] = ['a', 'b'].map(key => ({ kind: 'asset', key: `assets/${key}.bin`, objectHash: null, size: 0, mime: '', name: key, ext: 'bin', metadata: {} }))

export async function verifyContractReadback(store: PersistentDataStore): Promise<void> {
    const root = await store.readRoot()
    equal(root.value.username, 'contract-committed', 'delta survives reopen')
    equal((root.value as typeof root.value & { syntheticLarge?: string }).syntheticLarge, largeText, 'large transport survives reopen')
    equal((await store.readAssetAliasesByKeys('asset', aliases.map(alias => alias.key))).value, aliases, 'aliases survive reopen')
}

async function readContext(store: PersistentDataStore, characterId: string, conversationId: string, revision: number, beforeCharacter?: () => Promise<void>) {
    const lease = await store.acquireRevision(revision)
    const reader = new Proxy(lease, { get(target, key) {
        const value = Reflect.get(target, key, target)
        if (typeof value !== 'function') return value
        if (key !== 'readCharacter' || !beforeCharacter) return value.bind(target)
        return async (...args: unknown[]) => {
            const hook = beforeCharacter
            beforeCharacter = undefined
            await hook!()
            return value.apply(target, args)
        }
    } })
    try {
        return await readPinnedConversationContext(reader, normalizeConversationContextInput({
            characterId, conversationId, include: { character: true, lore: true }, messages: { limit: 128 },
        }), { selected: null, allowPrivate: true })
    } finally { await lease.release() }
}

export async function verifyPinnedConversationContext(store: PersistentDataStore, fixture: Database): Promise<void> {
    const character = fixture.characters[1]
    const chat = character.chats[1]
    const head = (await store.readRoot()).revision
    let committed = false
    const pinned = await readContext(store, character.chaId, chat.id!, head, async () => {
        const metadata = await store.readConversationMetadata(character.chaId, chat.id!)
        await store.commit({ expectedRevision: head,
            rootMutations: [{ type: 'set', key: 'syntheticContext', value: 'late' }],
            conversations: [{ type: 'replace-range', characterId: character.chaId, conversationId: chat.id!,
                start: chat.message.length, deleteCount: 0, messages: [{ role: 'char', data: 'late', chatId: 'context-late' }],
                conversation: { ...metadata!.value.conversation, name: 'late name' } }] })
        committed = true
    })
    if (!committed) throw new Error('context commit did not run')
    equal([pinned?.revision, pinned?.characterIndex, pinned?.chatIndex, pinned?.messageCount], [head, 1, 1, chat.message.length], 'context pinned position')
    equal([pinned?.character?.chaId, pinned?.conversation.name], [character.chaId, chat.name], 'context pinned records')
    equal(pinned?.messages?.messages.map(message => message.chatId), chat.message.map(message => message.chatId), 'context pinned window')
    const after = await readContext(store, character.chaId, chat.id!, head + 1)
    equal([after?.revision, after?.messageCount, after?.conversation.name], [head + 1, chat.message.length + 1, 'late name'], 'context after commit')
}

async function planPatch(store: PersistentDataStore, input: Record<string, unknown>) {
    const request = normalizeConversationPatchInput(input)
    const lease = await store.acquireRevision((await store.readRoot()).revision)
    try { return { request, revision: lease.revision, planned: await planStoredConversationPatch(lease, request) } }
    finally { await lease.release() }
}

async function commitPatch(store: PersistentDataStore, input: Record<string, unknown>, ranges: number) {
    const { request, revision, planned } = await planPatch(store, input)
    if (planned.kind !== 'apply') throw new Error(`patch conflict ${canonical(planned.conflict)}`)
    const prepared = prepareConversationPatchCommit(request, planned)
    equal(prepared.conversations.map(range => range.type === 'replace-range' ? [range.start, range.deleteCount] : null),
        planned.runs.map(run => [run.start, run.messages.length]), 'patch ranges preserve counts')
    equal(prepared.conversations.length, ranges, 'patch range count')
    const committed = await store.commit({ expectedRevision: revision, unitMutations: [...prepared.unitMutations], conversations: [...prepared.conversations] })
    equal(committed.revision, revision + 1, 'patch commits once')
    return committed.revision
}

export async function verifyConversationPatch(store: PersistentDataStore, fixture: Database): Promise<void> {
    const characterId = fixture.characters[0].chaId
    const conversationId = fixture.characters[0].chats[0].id!
    const target = { characterId, conversationId }
    const messages: Message[] = [
        { role: 'user', data: 'u0' },
        { role: 'char', data: 'c1', chatId: 'patch-c1' },
        ...Array.from({ length: 16 }, (_, offset): Message => ({ role: 'char', data: `f${offset + 2}`, chatId: `patch-f${offset + 2}` })),
        { role: 'user', data: 'u18', chatId: 'patch-dup' },
        { role: 'char', data: 'c19', chatId: 'patch-c19' },
        { role: 'user', data: 'u20', chatId: 'patch-dup' },
    ]
    const metadata = await store.readConversationMetadata(characterId, conversationId)
    await store.commit({ expectedRevision: metadata!.revision, conversations: [{ type: 'replace-range', ...target,
        start: 0, deleteCount: metadata!.value.totalMessages, messages, conversation: { ...metadata!.value.conversation, scriptstate: { $a: 1 } } }] })
    const readBack = async (label: string, scriptstate: Record<string, unknown>) => {
        const stored = await store.readConversation(characterId, conversationId)
        equal(stored?.value.message, messages, `${label} messages`)
        equal(stored?.value.scriptstate, scriptstate, `${label} chat variables`)
    }

    await commitPatch(store, { ...target, mutationId: 'runs',
        messages: [{ index: 19, messageId: 'patch-c19', set: { __x: 19 } },
            { index: 1, messageId: 'patch-c1', set: { __tr: { text: 't', parts: [1, 2] } } }],
        chatVariables: [{ key: '$a', value: 2 }, { key: '$b', value: 'x' }] }, 2)
    messages[1] = { ...messages[1], __tr: { text: 't', parts: [1, 2] } } as Message
    messages[19] = { ...messages[19], __x: 19 } as Message
    await readBack('separate runs', { $a: 2, $b: 'x' })

    await commitPatch(store, { ...target, mutationId: 'positions',
        messages: [{ index: 20, messageId: 'patch-dup', set: { __x: 20 } }, { index: 18, messageId: 'patch-dup', set: { __y: true } },
            { index: 19, messageId: 'patch-c19', set: { data: 'c19 edited', __x: undefined } }, { index: 0, messageId: null, set: { __x: 0 } }] }, 2)
    messages[0] = { ...messages[0], __x: 0 } as Message
    messages[18] = { ...messages[18], __y: true } as Message
    messages[19] = { role: 'char', data: 'c19 edited', chatId: 'patch-c19' }
    messages[20] = { ...messages[20], __x: 20 } as Message
    await readBack('repeated and missing IDs', { $a: 2, $b: 'x' })

    // A unique ID finds its message outside the window read at the addressed position, or past the end.
    await commitPatch(store, { ...target, mutationId: 'moved',
        messages: [{ index: 1, messageId: 'patch-c19', set: { __m: 1 } }, { index: messages.length, messageId: 'patch-c1', set: { __m: 2 } }] }, 2)
    messages[1] = { ...messages[1], __m: 2 } as Message
    messages[19] = { ...messages[19], __m: 1 } as Message
    await readBack('moved IDs', { $a: 2, $b: 'x' })

    const conflicts = await Promise.all([
        { ...target, mutationId: 'missing-id', messages: [{ index: 2, messageId: 'patch-missing', set: { __x: 1 } }] },
        { ...target, mutationId: 'repeated-elsewhere', messages: [{ index: 2, messageId: 'patch-dup', set: { __x: 1 } }] },
        { ...target, mutationId: 'unnamed-elsewhere', messages: [{ index: 1, messageId: null, set: { __x: 1 } }] },
        { ...target, conversationId: 'patch-missing', mutationId: 'missing', messages: [] },
    ].map(async input => (await planPatch(store, input)).planned))
    equal(conflicts, [
        { kind: 'conflict', conflict: { target: 'message', reason: 'not-found', index: 2 } },
        { kind: 'conflict', conflict: { target: 'message', reason: 'not-found', index: 2 } },
        { kind: 'conflict', conflict: { target: 'message', reason: 'not-found', index: 1 } },
        { kind: 'conflict', conflict: { target: 'conversation', reason: 'not-found' } },
    ], 'addressing conflicts')
    await readBack('after conflicts', { $a: 2, $b: 'x' })
}

export async function runContractScenarios(store: PersistentDataStore, fixture: Database, reopen: () => Promise<PersistentDataStore>): Promise<void> {
    const imported = await store.replaceFromDatabase(structuredClone(fixture))
    const before = await store.readRoot()
    await store.commitWorkingSetChangeCursor?.(imported.revision)
    const lease = await store.acquireRevision(imported.revision)
    try {
        const committed = await store.commit({ expectedRevision: imported.revision,
            rootMutations: [{ type: 'set', key: 'username', value: 'contract-committed' }], assetAliases: aliases })
        equal(committed.revision, imported.revision + 1, 'delta revision advances once')
        equal(await lease.readRoot(), before, 'lease isolates root from later commit')
        equal((await lease.listAssetAliases({ limit: 1 })).items, [], 'lease isolates aliases')
        let rejected: unknown
        try { await store.commit({ expectedRevision: imported.revision, rootMutations: [{ type: 'set', key: 'username', value: 'rejected' }] }) }
        catch (error) { rejected = error }
        if (!(rejected instanceof RevisionConflictError)) throw new Error('delta conflict type')
        equal([rejected.expectedRevision, rejected.actualRevision], [imported.revision, committed.revision], 'delta conflict fields')
        const first = await store.queryCharacters({ order: 'configured', trash: false, limit: 1 })
        if (!first.nextCursor) throw new Error('character cursor missing')
        const second = await store.queryCharacters({ order: 'configured', trash: false, limit: 1, cursor: first.nextCursor })
        equal([...first.items, ...second.items].map(item => item.id), fixture.characters.slice(0, 2).map(item => item.chaId), 'character pagination')
        const aliasPage = await store.listAssetAliases({ limit: 1 })
        if (!aliasPage.nextCursor) throw new Error('asset cursor missing')
        const aliasTail = await store.listAssetAliases({ limit: 1, cursor: aliasPage.nextCursor })
        equal([...aliasPage.items, ...aliasTail.items], aliases, 'asset pagination')
        const current = await store.acquireRevision(committed.revision)
        try {
            if (current.readWorkingSetChangePage) {
                const changes = await current.readWorkingSetChangePage(imported.revision, null, 1)
                equal(changes.length, 1, 'change page bound')
                const tail = await current.readWorkingSetChangePage(imported.revision, changes[0], 1)
                if (tail.some(item => canonical(item) === canonical(changes[0]))) throw new Error('change cursor repeated')
            }
        } finally { await current.release() }
        await store.commit({ expectedRevision: committed.revision, rootMutations: [{ type: 'set', key: 'syntheticLarge', value: largeText }] })
    } finally { await lease.release() }
    let released: unknown
    try { await lease.readRoot() } catch (error) { released = error }
    if (!(released instanceof SnapshotReleasedError)) throw new Error('released lease still readable')
    await verifyPinnedConversationContext(store, fixture)
    await verifyConversationPatch(store, fixture)
    await verifyContractReadback(await reopen())
}
