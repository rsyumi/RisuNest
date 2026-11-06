import { expect, it, vi } from 'vitest'
import { captureRoot, makeDatabase, SaveCoordinator } from './saveCoordinator.testSupport'
import { applyCharacterDetailPatch, type CharacterDetailPatch } from './characterDetailPatch'
import { createStoreStub } from './persistentDataStore.testSupport'
import type { CharacterDetail, WorkingSetCommit } from './persistentDataStore'
import type { WindowedConversationPersistenceAuthority } from './saveCoordinator'
import type { character } from './database.svelte'

// The stored character record. Any conversation read is an unexpected call on the stub.
function makeStoredCharacter(initial: CharacterDetail) {
    let revision = 2
    let detail: CharacterDetail | null = structuredClone(initial)
    const commit = vi.fn(async (input: WorkingSetCommit) => {
        for (const mutation of input.unitMutations ?? []) {
            const [kind, id, field] = JSON.parse(mutation.key) as string[]
            if (kind !== 'character' || id !== detail?.chaId) throw new Error(`Unexpected unit ${mutation.key}`)
            const record = detail as unknown as Record<string, unknown>
            if (mutation.type === 'delete') delete record[field]
            else record[field] = structuredClone(mutation.value)
        }
        return { revision: ++revision }
    })
    const readCharacter = vi.fn(async (id: string) => detail?.chaId === id ? { revision, value: structuredClone(detail) } : null)
    const store = createStoreStub({ commit, readCharacter })
    return { commit, readCharacter, store, stored: () => detail, remove: () => { detail = null } }
}

function makeWindowedSelection() {
    const database = makeDatabase()
    const projectedConversation: Record<string, unknown> = { id: 'two', name: 'Two', localLore: [], note: '' }
    Object.defineProperty(projectedConversation, 'message', {
        enumerable: true,
        get: () => { throw new Error('projected messages must not be traversed') },
    })
    const selected = {
        type: 'character', chaId: 'char-a', name: 'Alpha', modules: ['module-a'],
        chats: [projectedConversation],
    } as unknown as character
    const storage = makeStoredCharacter({ type: 'character', chaId: 'char-a', name: 'Alpha', modules: ['module-a'] } as CharacterDetail)
    let authority: WindowedConversationPersistenceAuthority = {
        kind: 'windowed', characterId: 'char-a', conversationId: 'two', sessionToken: 'windowed-session' as never,
        storeRevision: 2, persistedSessionVersion: 0, sessionVersion: 0, totalMessages: 10_000,
    }
    const coordinator: SaveCoordinator = new SaveCoordinator({
        store: storage.store,
        captureRoot: () => captureRoot(database),
        captureSelectedCharacter: () => selected,
        captureSelectedConversationAuthority: () => authority,
        replaceDatabase: () => undefined,
        // As the working set does, a revision of this store moves the windowed selection along.
        onStorageOnlyRevision: (revision) => {
            authority = { ...authority, storeRevision: revision }
            expect(coordinator.advanceWindowedSelectedConversationRevision(revision, authority)).toBe(true)
        },
    })
    coordinator.initialize(2, database)
    expect(coordinator.adoptWindowedSelectedConversation(2, coordinator.mutationGeneration, selected, authority)).toBe(true)
    return { ...storage, coordinator, selected, authority: () => authority }
}

function makeCompleteSelection() {
    const database = makeDatabase()
    const selected = database.characters[0]
    const storage = makeStoredCharacter({ type: 'character', chaId: 'char-a', name: 'Alpha' } as CharacterDetail)
    const coordinator = new SaveCoordinator({
        store: storage.store,
        captureRoot: () => captureRoot(database),
        captureSelectedCharacter: () => selected,
        replaceDatabase: () => undefined,
    })
    coordinator.initialize(2, database)
    return { ...storage, coordinator, selected }
}

it('stores a field of the windowed selected character without reading its conversation and leaves the selection clean', async () => {
    const { coordinator, commit, selected, stored, authority } = makeWindowedSelection()
    const publish = vi.fn((patch: CharacterDetailPatch) => { applyCharacterDetailPatch(selected, patch) })
    expect(await coordinator.mutateCharacterDetailFields('char-a', 'toggle-memory', ({ character }) => {
        character.supaMemory = true
        character.modules = [...character.modules!, 'module-b']
    }, publish)).toBe(true)

    expect(commit).toHaveBeenCalledExactlyOnceWith({ expectedRevision: 2, unitMutations: [
        { key: JSON.stringify(['character', 'char-a', 'modules']), type: 'set', value: ['module-a', 'module-b'] },
        { key: JSON.stringify(['character', 'char-a', 'supaMemory']), type: 'set', value: true },
    ] })
    expect(publish).toHaveBeenCalledExactlyOnceWith({ modules: ['module-a', 'module-b'], supaMemory: true })
    expect(stored()).toMatchObject({ supaMemory: true, modules: ['module-a', 'module-b'] })
    expect(authority().storeRevision).toBe(3)
    // The published value is the selection's own copy.
    selected.modules!.push('later-ui-edit')
    expect(stored()?.modules).toEqual(['module-a', 'module-b'])
    selected.modules!.pop()

    // The baseline holds the stored fields, so the selection has nothing left to save.
    await coordinator.flushPendingData('verify-clean-detail')
    expect(commit).toHaveBeenCalledOnce()
    expect(coordinator.hasPendingPersistenceWork).toBe(false)
})

it('removes a field and stores later edits of the same windowed selection in turn', async () => {
    const { coordinator, commit, selected, stored } = makeWindowedSelection()
    const publish = (patch: CharacterDetailPatch) => { applyCharacterDetailPatch(selected, patch) }
    await coordinator.mutateCharacterDetailFields('char-a', 'set-memory', ({ character }) => { character.supaMemory = true }, publish)
    await coordinator.mutateCharacterDetailFields('char-a', 'clear-modules', ({ character }) => { delete character.modules }, publish)
    expect(commit.mock.calls.map(([input]) => input)).toEqual([
        { expectedRevision: 2, unitMutations: [{ key: JSON.stringify(['character', 'char-a', 'supaMemory']), type: 'set', value: true }] },
        { expectedRevision: 3, unitMutations: [{ key: JSON.stringify(['character', 'char-a', 'modules']), type: 'delete' }] },
    ])
    expect(selected).not.toHaveProperty('modules')
    expect(stored()).not.toHaveProperty('modules')
    await coordinator.flushPendingData('verify-clean-detail')
    expect(commit).toHaveBeenCalledTimes(2)
    expect(coordinator.hasPendingPersistenceWork).toBe(false)
})

it('stores a field of a complete selected character and advances its baseline', async () => {
    const { coordinator, commit, selected, stored } = makeCompleteSelection()
    const publish = vi.fn((patch: CharacterDetailPatch) => { applyCharacterDetailPatch(selected, patch) })
    expect(await coordinator.mutateCharacterDetailFields('char-a', 'toggle-memory', ({ character }) => {
        character.supaMemory = true
    }, publish)).toBe(true)
    expect(publish).toHaveBeenCalledExactlyOnceWith({ supaMemory: true })
    expect(selected.supaMemory).toBe(true)
    expect(stored()?.supaMemory).toBe(true)
    await coordinator.flushPendingData('verify-clean-detail')
    expect(commit).toHaveBeenCalledOnce()
    expect(coordinator.hasPendingPersistenceWork).toBe(false)
})

it('commits and publishes nothing when the stored record already holds the change', async () => {
    const { coordinator, commit } = makeCompleteSelection()
    const publish = vi.fn()
    expect(await coordinator.mutateCharacterDetailFields('char-a', 'same-name', ({ character }) => {
        character.name = 'Alpha'
    }, publish)).toBe(true)
    expect(commit).not.toHaveBeenCalled()
    expect(publish).not.toHaveBeenCalled()
})

it('resolves false for a character the store does not hold', async () => {
    const { coordinator, commit, remove } = makeCompleteSelection()
    remove()
    const mutate = vi.fn()
    const publish = vi.fn()
    expect(await coordinator.mutateCharacterDetailFields('char-a', 'missing', mutate, publish)).toBe(false)
    expect(mutate).not.toHaveBeenCalled()
    expect(commit).not.toHaveBeenCalled()
    expect(publish).not.toHaveBeenCalled()
})

it('refuses an edit of the chat folders or identity before committing', async () => {
    const { coordinator, commit } = makeWindowedSelection()
    const publish = vi.fn()
    await expect(coordinator.mutateCharacterDetailFields('char-a', 'folders', ({ character }) => {
        character.chatFolders = [{ id: 'folder', name: 'Folder', folded: false }]
    }, publish)).rejects.toThrow(TypeError)
    await expect(coordinator.mutateCharacterDetailFields('char-a', 'identity', ({ character }) => {
        character.chaId = 'char-b'
    }, publish)).rejects.toThrow(TypeError)
    expect(commit).not.toHaveBeenCalled()
    expect(publish).not.toHaveBeenCalled()
})
