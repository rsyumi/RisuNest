import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it } from 'vitest'
import type { Message } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { fixtureDatabase } from './tests/persistentDataFixtures'
import { WindowedConversationOverlay } from './windowedConversationOverlay'

const owner = { characterId: 'char-b', conversationId: 'conv-beta' }
function row(chatId: string, data = chatId): Message { return { chatId, data, role: 'char' } }

async function fixture(...rows: Message[]) {
    const indexedDB = new IDBFactory()
    const open = async () => {
        const store = new IndexedDbPersistentDataStore('windowed-overlay', indexedDB, IDBKeyRange)
        await store.open()
        return store
    }
    const local = await open()
    const remote = await open()
    const database = structuredClone(fixtureDatabase)
    database.characters[0].chats[0].message = rows
    const imported = await local.replaceFromDatabase(database)
    const overlay = new WindowedConversationOverlay(owner.characterId, owner.conversationId, imported.revision, rows.length)
    const messages = async () => (await remote.readConversation(owner.characterId, owner.conversationId))!.value.message
    const plan = async (prefix = overlay.freeze()) => {
        const reader = await local.acquireRevision((await local.readRoot()).revision)
        try {
            const metadata = await reader.readConversationMetadata(owner.characterId, owner.conversationId)
            return await overlay.plan(prefix, reader, metadata!.value.totalMessages)
        } finally { await reader.release() }
    }
    const save = async (prepared: Awaited<ReturnType<typeof plan>>) => {
        const revision = prepared.mutations.length ? (await local.commit({
            expectedRevision: prepared.revision, conversations: prepared.mutations,
        })).revision : prepared.revision
        overlay.acknowledge(prepared, revision)
    }
    const rewrite = async (...after: Message[]) => {
        const old = await messages()
        await remote.commit({ expectedRevision: (await remote.readRoot()).revision, conversations: [{
            type: 'replace-range', ...owner, start: 0, deleteCount: old.length, messages: after,
        }] })
    }
    const rebase = async () => {
        const reader = await local.acquireRevision((await local.readRoot()).revision)
        try {
            const metadata = await reader.readConversationMetadata(owner.characterId, owner.conversationId)
            expect(await overlay.rebase(reader, metadata!.value.totalMessages)).toBe(true)
        } finally { await reader.release() }
    }
    return { overlay, local, messages, plan, save, rewrite, rebase }
}

describe('windowed conversation overlay', () => {
    it('preserves logical first and after-anchor insertion in a reordered complete conversation', () => {
        const [a,b,c,x,y] = ['a','b','c','x','y'].map((id) => row(id))
        const overlay = new WindowedConversationOverlay(owner.characterId, owner.conversationId, 1, 3)
        const moved = overlay.bind(0,a)
        overlay.applyRange(0, [], [x], 1, { head: true })
        overlay.applyRange(2, [], [y], 2, { anchorBefore: true })
        const stored = [b,c,a]
        const projection = overlay.projectCompleteRows([a,b,c], stored)
        for (const mutation of projection.mutations) stored.splice(mutation.start, mutation.deleteCount, ...mutation.messages)
        expect(stored).toEqual([x,b,c,a,y])
        expect(projection.locate(moved)).toBe(3)
    })

    it('resolves positions again after two conflicts in the commit carrying the frozen edits', async () => {
        const [a,b,c] = ['a','b','c'].map((id) => row(id))
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture(a,b,c)
        overlay.applyRange(0, [a,b], [row('a','A'),row('b','B')], 1)
        overlay.applyRange(2, [], [row('before-c')], 2)
        const frozen = overlay.freeze()
        const first = await plan(frozen)
        await rewrite(a,c,row('b','duplicate'),b)
        await expect(save(first)).rejects.toThrow()
        await rebase()
        const second = await plan(frozen)
        expect(overlay.row(4)).toEqual({ storedIndex: 3 })
        await rewrite(c,b,a)
        await expect(save(second)).rejects.toThrow()
        await rebase()
        await save(await plan(frozen))
        expect(await messages()).toEqual([row('before-c'),c,row('b','B'),row('a','A')])
    })

    it('retains only the latest chunk on each side of a frozen append and follows its committed ID', async () => {
        const [a,x,y,z] = ['a','x','y','z'].map((id) => row(id))
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture(a)
        overlay.applyRange(1, [], [x], 1)
        const output = overlay.bind(1,x)
        const first = await plan()
        for (let version = 2; version <= 20; version++) overlay.applyRange(1, [x], [row('x', String(version))], version)
        overlay.applyRange(2, [], [y], 21)
        await save(first)
        expect(await messages()).toEqual([a,x])
        await rewrite(z,x,a)
        await rebase()
        expect(overlay.readTarget(output)).toEqual(row('x','20'))
        expect(overlay.locate(output)).toBe(1)
        await save(await plan())
        expect(await messages()).toEqual([z,row('x','20'),a,y])
    })

    it('cancels a pending append and deletes an append that its frozen prefix already committed', async () => {
        const [a,b,c] = ['a','b','c'].map((id) => row(id))
        const { overlay, plan, save, messages } = await fixture(a)
        overlay.applyRange(1, [], [b,c], 1)
        overlay.applyRange(2,[c],[],2)
        const initial = await plan()
        overlay.applyRange(1,[b],[],3)
        await save(initial)
        expect(await messages()).toEqual([a,b])
        expect(overlay.count).toBe(1)
        await save(await plan())
        expect(await messages()).toEqual([a])
    })

    it('keeps a post-capture colliding append hidden after the earlier prefix commits', async () => {
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture()
        overlay.applyRange(0, [], [row('a')], 1)
        const first = await plan()
        overlay.applyRange(1, [], [row('b','local')], 2)
        await rewrite(row('b','remote'))
        await expect(save(first)).rejects.toThrow()
        await rebase()
        await save(await plan(first.prefix))
        expect(overlay.count).toBe(2)
        expect(await messages()).toEqual([row('b','remote'),row('a')])
    })

    it('does not revive an ambiguous target, an unproven legacy edit, or a colliding append', async () => {
        const empty = row('','empty ID')
        const missing: Message = { role: 'user', data: 'missing ID' }
        const [a,b,c] = ['a','b','c'].map((id) => row(id))
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture(a,empty,missing,a)
        overlay.applyRange(0,[a],[row('a','local duplicate')],1)
        overlay.applyRange(1,[empty],[{ ...empty, data: 'local' }],2)
        overlay.applyRange(2,[missing],[{ ...missing, data: 'local' }],3)
        overlay.applyRange(4,[],[b,c],4)
        const first = await plan()
        await rewrite(empty,missing,a,row('b','remote'))
        await expect(save(first)).rejects.toThrow()
        await rebase()
        await save(await plan(first.prefix))
        expect(await messages()).toEqual([empty,missing,a,row('b','remote'),c])
    })

    it('saves explicit ID rewrites atomically and follows dependent edits and handles across acknowledgement', async () => {
        const [a,b,c] = ['a','b','c'].map((id) => row(id))
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture(a,b)
        const bound = overlay.bind(0,a)
        overlay.applyRange(0,[a],[c],1)
        overlay.applyRange(2,[],[row('c','collision')],2)
        const frozen = await plan()
        overlay.applyRange(0,[c],[row('c','dependent')],3)
        await save(frozen)
        expect(await messages()).toEqual([c,b])
        expect(overlay.readTarget(bound)).toEqual(row('c','dependent'))
        await rewrite(b,c)
        await rebase()
        expect(overlay.locate(bound)).toBe(1)
        await save(await plan())
        expect(await messages()).toEqual([b,row('c','dependent')])
    })

    it('keeps independent deletes on either side of an inserted row and rejects a split compound', async () => {
        const [a,b,c,d,e,x] = ['a','b','c','d','e','x'].map((id) => row(id))
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture(a,b,c,d,e)
        overlay.applyRange(1,[b,c],[c,b],1)
        overlay.applyRange(0,[a],[],2)
        overlay.applyRange(2,[d],[],3)
        const first = await plan()
        await rewrite(a,d,b,x,c,e)
        await expect(save(first)).rejects.toThrow()
        await rebase()
        await save(await plan(first.prefix))
        expect(await messages()).toEqual([b,x,c,e])
    })

    it('requires the end for a tail splice and preserves a moved intact compound group', async () => {
        const [a,b,c,d,e,x] = ['a','b','c','d','e','x'].map((id) => row(id))
        const { overlay, plan, save, rewrite, rebase, messages } = await fixture(a,b,c,d,e)
        overlay.applyRange(0,[a,b],[b,a],1)
        overlay.applyRange(3,[d,e],[x],2,{tail:true,compound:true})
        const first = await plan()
        await rewrite(c,a,b,d,e,x)
        await expect(save(first)).rejects.toThrow()
        await rebase()
        await save(await plan(first.prefix))
        expect(await messages()).toEqual([c,b,a,d,e,x])
    })

    it('assigns an ID only on an admitted legacy edit, including rollback, and retains a supplied ID', async () => {
        const legacy: Message = { role: 'user', data: 'legacy' }
        const { overlay, plan, save, messages } = await fixture(legacy)
        const undo = overlay.applyRange(0,[legacy],[{ ...legacy, data:'temporary' }],1)
        undo()
        expect(overlay.dirty).toBe(false)
        expect(await messages()).toEqual([legacy])
        overlay.applyRange(0,[legacy],[{ ...legacy, data:'edited' }],1)
        overlay.applyRange(1,[],[row('supplied')],2)
        await save(await plan())
        const [edited,added] = await messages()
        expect(edited).toMatchObject({ role:'user', data:'edited', chatId: expect.stringMatching(/^[0-9a-f-]{36}$/) })
        expect(added).toEqual(row('supplied'))
    })
})
