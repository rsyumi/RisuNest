import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    pick: vi.fn(), discard: vi.fn(), read: vi.fn(),
    character: vi.fn(), module: vi.fn(), jsonCharacter: vi.fn(), jsonModule: vi.fn(),
    run: vi.fn(), source: vi.fn(), status: vi.fn(),
    controller: undefined as AbortController | undefined,
}))
vi.mock('./iosFiles', () => ({
    pickIOSFile: mocks.pick, discardIOSFile: mocks.discard,
    exportIOSFile: vi.fn(), getIOSPublication: vi.fn(),
}))
vi.mock('@tauri-apps/plugin-fs', () => ({ readFile: mocks.read }))
vi.mock('../characterCards', () => ({
    importPreparedNativeCharacterContent: mocks.character, importCharacterProcess: mocks.jsonCharacter,
}))
vi.mock('../process/modules', () => ({
    importPreparedNativeModuleContent: mocks.module, importModuleData: mocks.jsonModule,
}))
vi.mock('./database.svelte', () => ({ getDatabase: () => ({ characters: [{ chaId: 'json-character' }] }) }))
vi.mock('./nativeFileJobManager', () => ({ runSharedNativeFileOperation: mocks.run }))
import { importIOSContentFromPicker } from './iosContentPicker'

const picked = { path: '/synthetic/ios-file-staging/source', name: 'card.charx', bytes: 10 }

describe('iOS native content picker', () => {
    beforeEach(() => {
        vi.resetAllMocks()
        mocks.controller = new AbortController()
        mocks.pick.mockResolvedValue(picked)
        mocks.discard.mockResolvedValue(undefined)
        mocks.character.mockResolvedValue({ kind: 'imported', value: 'native-character' })
        mocks.module.mockResolvedValue({ kind: 'imported', value: 'native-module' })
        mocks.jsonCharacter.mockResolvedValue(0)
        mocks.read.mockResolvedValue(new TextEncoder().encode('{}'))
        mocks.run.mockImplementation(async (_kind, _key, operation) => operation({
            signal: mocks.controller!.signal, onStatus: mocks.status, setSource: mocks.source,
        }))
    })

    it.each(['card.png', 'card.charx', 'card.jpeg'])('passes %s as an owned native path and cleans it only after activation', async name => {
        mocks.pick.mockResolvedValue({ ...picked, name, bytes: 1024 ** 3 })
        mocks.character.mockImplementation(async (input, options) => {
            expect(mocks.discard).not.toHaveBeenCalled()
            expect(input).toEqual({ source: { type: 'desktopPath', path: picked.path }, displayName: name })
            expect(options).toEqual({ signal: mocks.controller!.signal, onStatus: mocks.status })
            return { kind: 'imported', value: 'native-character' }
        })
        await expect(importIOSContentFromPicker('character')).resolves.toBe('native-character')
        expect(mocks.run).toHaveBeenCalledWith('import', 'content-picker', expect.any(Function), {
            presentation: 'dialog', format: 'content',
        })
        expect(mocks.read).not.toHaveBeenCalled()
        expect(mocks.source).toHaveBeenCalledWith({ name, bytes: 1024 ** 3 })
        expect(mocks.discard).toHaveBeenCalledWith(picked.path)
    })

    it.each([['module', 'source.risum'], ['module', 'source.charx'], ['character', 'source.RISUM']] as const)
    ('routes %s / %s to prepared module activation', async (destination, name) => {
        mocks.pick.mockResolvedValue({ ...picked, name })
        await expect(importIOSContentFromPicker(destination)).resolves.toBe('native-module')
        expect(mocks.module).toHaveBeenCalledOnce()
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.read).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it.each(['character', 'module'] as const)('preserves the %s JSON compatibility path', async destination => {
        mocks.pick.mockResolvedValue({ ...picked, name: 'source.JSON' })
        await expect(importIOSContentFromPicker(destination)).resolves.toBe(destination === 'character' ? 'json-character' : 'module')
        expect(mocks.read).toHaveBeenCalledWith(picked.path)
        expect(destination === 'character' ? mocks.jsonCharacter : mocks.jsonModule).toHaveBeenCalledOnce()
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.module).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('rejects oversized metadata before reading it into the renderer', async () => {
        mocks.pick.mockResolvedValue({ ...picked, name: 'large.json', bytes: 128 * 1024 * 1024 + 1 })
        await expect(importIOSContentFromPicker('character')).rejects.toMatchObject({ code: 'source-too-large' })
        expect(mocks.read).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('does not turn a native rejection into a whole-file fallback', async () => {
        const error = new Error('synthetic unsupported content')
        mocks.character.mockRejectedValue(error)
        await expect(importIOSContentFromPicker('character')).rejects.toBe(error)
        expect(mocks.read).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('discards a source when cancellation arrives just after the picker', async () => {
        mocks.pick.mockImplementation(async () => {
            mocks.controller!.abort()
            return picked
        })
        await expect(importIOSContentFromPicker('character')).rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('cleans up a declined native import without reporting success', async () => {
        mocks.character.mockResolvedValue({ kind: 'declined' })
        await expect(importIOSContentFromPicker('character')).resolves.toBeNull()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('returns cancellation without inventing a staging file to remove', async () => {
        mocks.pick.mockResolvedValue(null)
        await expect(importIOSContentFromPicker('character')).resolves.toBeNull()
        expect(mocks.discard).not.toHaveBeenCalled()
    })

    it('does not report an activated import as failed when staging cleanup fails', async () => {
        mocks.discard.mockRejectedValue(new Error('synthetic cleanup failure'))
        const warning = vi.spyOn(console, 'warn').mockImplementation(() => {})
        try {
            await expect(importIOSContentFromPicker('character')).resolves.toBe('native-character')
            expect(warning).toHaveBeenCalledOnce()
        } finally {
            warning.mockRestore()
        }
    })
})
