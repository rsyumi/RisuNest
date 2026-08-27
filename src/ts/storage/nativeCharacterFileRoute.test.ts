import { describe, expect, it, vi } from 'vitest'

import {
    importAndroidNativeCharacterSpool,
    importDesktopNativeCharacterPath,
    importDesktopNativeCharacterFromPicker,
    type NativeCharacterFileRouteDependencies,
} from './nativeCharacterFileRoute'
import { NativeFileJobError } from './nativeFileJobs'

function dependencies(
    overrides: Partial<NativeCharacterFileRouteDependencies<string>> = {},
): NativeCharacterFileRouteDependencies<string> {
    return {
        chooseDesktopPath: vi.fn(async () => 'C:\\chosen\\card.charx'),
        readDesktopPath: vi.fn(async () => Uint8Array.from([1, 2, 3])),
        nativeEnabled: () => true,
        nativeImport: vi.fn(async () => ({
            kind: 'imported' as const,
            value: 'native-card',
        })),
        legacyImport: vi.fn(async () => 'legacy-card'),
        ...overrides,
    }
}

describe('native character file route', () => {
    it('routes a selected PNG card through native preparation without reading its bytes', async () => {
        const deps = dependencies()

        await expect(importDesktopNativeCharacterPath(
            'C:\\chosen\\card.PNG',
            deps,
        )).resolves.toEqual({
            kind: 'imported',
            mode: 'native',
            value: 'native-card',
        })

        expect(deps.nativeImport).toHaveBeenCalledWith({
            source: { type: 'desktopPath', path: 'C:\\chosen\\card.PNG' },
            displayName: 'card.PNG',
        })
        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })

    it('falls back to the existing PNG importer when the native parser reaches its bounded limit', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => {
                throw new NativeFileJobError('native-limit', 'PNG metadata is too large')
            }),
        })

        await expect(importDesktopNativeCharacterPath(
            'C:\\chosen\\large.png',
            deps,
        )).resolves.toEqual({
            kind: 'imported',
            mode: 'legacy',
            value: 'legacy-card',
        })

        expect(deps.nativeImport).toHaveBeenCalledOnce()
        expect(deps.readDesktopPath).toHaveBeenCalledWith('C:\\chosen\\large.png')
    })

    it('uses one desktop picker and the selected path for a pre-activation compatibility fallback', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => {
                throw new NativeFileJobError('capability-unavailable', 'Native content import is unavailable')
            }),
        })

        await expect(importDesktopNativeCharacterFromPicker(deps)).resolves.toEqual({
            kind: 'imported',
            mode: 'legacy',
            value: 'legacy-card',
        })

        expect(deps.chooseDesktopPath).toHaveBeenCalledOnce()
        expect(deps.nativeImport).toHaveBeenCalledWith({
            source: { type: 'desktopPath', path: 'C:\\chosen\\card.charx' },
            displayName: 'card.charx',
        })
        expect(deps.readDesktopPath).toHaveBeenCalledWith('C:\\chosen\\card.charx')
        expect(deps.legacyImport).toHaveBeenCalledWith({
            name: 'card.charx',
            data: Uint8Array.from([1, 2, 3]),
        })
    })

    it('does not fall back after native mapping or persistence has begun', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => { throw new Error('revision conflict') }),
        })

        await expect(importDesktopNativeCharacterFromPicker(deps)).rejects.toThrow('revision conflict')

        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })

    it('uses legacy compatibility for an off-spec native character card result', async () => {
        const error = Object.assign(new TypeError('Unsupported card'), {
            name: 'UnsupportedPreparedNativeCharacterCardError',
            code: 'unsupported-character-card',
        })
        const deps = dependencies({
            nativeImport: vi.fn(async () => {
                throw error
            }),
        })

        await expect(importDesktopNativeCharacterFromPicker(deps)).resolves.toEqual({
            kind: 'imported',
            mode: 'legacy',
            value: 'legacy-card',
        })

        expect(deps.readDesktopPath).toHaveBeenCalledWith('C:\\chosen\\card.charx')
    })

    it('does not treat a matching arbitrary error code as a pre-activation fallback', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => { throw { code: 'unsupported-format' } }),
        })

        await expect(importDesktopNativeCharacterFromPicker(deps)).rejects.toEqual({
            code: 'unsupported-format',
        })

        expect(deps.readDesktopPath).not.toHaveBeenCalled()
    })

    it('treats a low-level decline as a handled non-import', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => ({ kind: 'declined' as const })),
        })

        await expect(importDesktopNativeCharacterFromPicker(deps)).resolves.toEqual({ kind: 'declined' })

        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })

    it('returns the stable destination-required result for an ordinary JPEG without activation or fallback', async () => {
        const deps = dependencies({
            chooseDesktopPath: vi.fn(async () => 'C:\\chosen\\portrait.jpeg'),
            nativeImport: vi.fn(async () => {
                throw new NativeFileJobError('destination-required', 'JPEG is not a character card')
            }),
        })

        await expect(importDesktopNativeCharacterFromPicker(deps)).resolves.toEqual({
            kind: 'destination-required',
        })

        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })

    it('passes an Android spool source to the same native seam without reading bytes', async () => {
        const deps = dependencies()

        await expect(importAndroidNativeCharacterSpool({
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.json',
        }, deps)).resolves.toEqual({ kind: 'imported', value: 'native-card' })

        expect(deps.nativeImport).toHaveBeenCalledWith({
            source: { type: 'androidSpool', token: '11111111-1111-4111-8111-111111111111' },
            displayName: 'card.json',
        })
        expect(deps.readDesktopPath).not.toHaveBeenCalled()
    })

    it('returns a capability fallback before claiming an inactive Android spool', async () => {
        const deps = dependencies({ nativeEnabled: () => false })

        await expect(importAndroidNativeCharacterSpool({
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.charx',
        }, deps)).resolves.toEqual({ kind: 'capability-unavailable' })

        expect(deps.nativeImport).not.toHaveBeenCalled()
        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })

    it('keeps an Android spool intact when the native start gate is unavailable', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => {
                throw new NativeFileJobError(
                    'capability-unavailable',
                    'Native content import is unavailable',
                )
            }),
        })

        await expect(importAndroidNativeCharacterSpool({
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.charx',
        }, deps)).resolves.toEqual({ kind: 'capability-unavailable' })

        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })

    it('maps the stable ordinary JPEG native error to the Android destination result', async () => {
        const deps = dependencies({
            nativeImport: vi.fn(async () => {
                throw new NativeFileJobError(
                    'unsupported-without-destination',
                    'ordinary JPEG import requires an asset destination',
                )
            }),
        })

        await expect(importAndroidNativeCharacterSpool({
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'portrait.jpeg',
        }, deps)).resolves.toEqual({ kind: 'destination-required' })

        expect(deps.readDesktopPath).not.toHaveBeenCalled()
        expect(deps.legacyImport).not.toHaveBeenCalled()
    })
})
