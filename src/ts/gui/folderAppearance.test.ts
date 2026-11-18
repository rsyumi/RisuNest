import { describe, expect, it } from 'vitest'
import type { folder } from '../storage/database.svelte'
import {
    applyFolderDraft,
    autoFolderTextColor,
    chosenTextContrast,
    contrastRatio,
    folderDraftFrom,
    nearestFolderPreset,
    normalizeHexColor,
    releaseFolder,
    resolveFolderAppearance,
} from './folderAppearance'

const theme = { bgcolor: '#282a36', darkbg: '#21222c' }
const stored = (fields: Partial<folder> = {}): folder => ({ name: '판타지', data: ['a', 'b'], color: 'blue', id: 'f1', ...fields })

describe('colour helpers', () => {
    it('accepts only hex colours and stores them as lowercase #rrggbb', () => {
        expect(normalizeHexColor('#ABC')).toBe('#aabbcc')
        expect(normalizeHexColor(' #0F766E ')).toBe('#0f766e')
        for (const value of ['red', '#12345', '#0f766e80', 'url(x)', 'linear-gradient(#000, #fff)', 3, null]) expect(normalizeHexColor(value)).toBeNull()
    })

    it('chooses the more readable text colour and adds a shadow only under 3:1', () => {
        expect(contrastRatio('#ffffff', '#000000')).toBeCloseTo(21)
        expect(autoFolderTextColor(['#fde68a'])).toMatchObject({ color: '#111827', shadow: null })
        expect(autoFolderTextColor(['#0f766e'])).toMatchObject({ color: '#ffffff', shadow: null })
        const both = autoFolderTextColor(['#fef3c7', '#1e3a8a'])
        expect(both.contrast).toBeLessThan(3)
        expect(both.shadow).not.toBeNull()
        expect(both.shadow).not.toBe(both.color)
    })

    it('keeps the nearest preset by hue and greys as default', () => {
        expect(nearestFolderPreset(['#ef4444'])).toBe('red')
        expect(nearestFolderPreset(['#0f766e'])).toBe('green')
        expect(nearestFolderPreset(['#a855f7'])).toBe('purple')
        expect(nearestFolderPreset(['#808080'])).toBe('default')
        expect(nearestFolderPreset(['#2563eb', '#1d4ed8'])).toBe('blue')
    })

    it('measures a chosen text colour against the preset as painted over the sidebar', () => {
        const preset = resolveFolderAppearance(stored({ textColor: '#000000' }))
        expect(chosenTextContrast(preset, theme)).toBeLessThan(3)
        expect(chosenTextContrast({ ...preset, textColor: '#ffffff' }, theme)).toBeGreaterThan(3)
        expect(chosenTextContrast({ ...preset, textColor: null }, theme)).toBeNull()
        expect(chosenTextContrast(resolveFolderAppearance(stored({ colorStops: ['#ffffff', '#000000'], textColor: '#ffffff' })), theme)).toBe(1)
    })
})

describe('stored folder appearance', () => {
    it('falls back like older builds for missing or invalid fields', () => {
        expect(resolveFolderAppearance(stored())).toEqual({ iconType: 'text', text: '판타지', preset: 'blue', stops: null, angle: 135, textColor: null })
        expect(resolveFolderAppearance(stored({ imgFile: 'assets/f.png' })).iconType).toBe('image')
        expect(resolveFolderAppearance(stored({ iconType: 'image' })).iconType).toBe('text')
        expect(resolveFolderAppearance(stored({ imgFile: 'assets/f.png', iconType: 'glyph' })).iconType).toBe('glyph')
        expect(resolveFolderAppearance(stored({ color: '', colorStops: ['#zzzzzz'], colorAngle: 30, textColor: 'white', iconText: '  ' })))
            .toMatchObject({ preset: 'default', stops: null, angle: 135, textColor: null, text: '판타지' })
        expect(resolveFolderAppearance(stored({ iconText: '판타\n지' })).text).toBe('판타\n지')
        // Upstream's image reset leaves this shape; it shows the name and saves back unchanged.
        const reset = stored({ imgFile: null as unknown as string, img: '' })
        expect(resolveFolderAppearance(reset).iconType).toBe('text')
        expect(applyFolderDraft(reset, folderDraftFrom(reset, theme))).toEqual(reset)
    })

    it('saves only what differs from the absent meaning and keeps every other key', () => {
        const original = { ...stored({ imgFile: 'assets/f.png', img: 'blob:f' }), unrelated: 1 }
        const draft = folderDraftFrom(original, theme)
        expect(applyFolderDraft(original, draft)).toEqual(original)

        const solid = applyFolderDraft(original, { ...draft, name: '로맨스', iconText: '로', iconType: 'text', colorMode: 'solid', solid: '#0F766E', textMode: 'manual', textColor: '#FFFFFF' })
        expect(solid).toEqual({ ...original, name: '로맨스', iconText: '로', iconType: 'text', colorStops: ['#0f766e'], color: 'green', textColor: '#ffffff' })

        const gradient = applyFolderDraft(solid, { ...folderDraftFrom(solid, theme), colorMode: 'gradient', gradient: ['#f97316', '#ec4899'], angle: 90, iconType: 'image', iconText: ' ', textMode: 'auto' })
        expect(gradient).toEqual({ ...original, name: '로맨스', colorStops: ['#f97316', '#ec4899'], colorAngle: 90, color: 'red' })

        const back = applyFolderDraft(gradient, { ...folderDraftFrom(gradient, theme), colorMode: 'preset', preset: 'pink', iconType: 'glyph' })
        expect(back).toEqual({ ...original, name: '로맨스', color: 'pink', iconType: 'glyph' })
    })

    it('stores 이미지 without an image as text and clears a removed image the way upstream resets it', () => {
        const plain = stored()
        expect(applyFolderDraft(plain, { ...folderDraftFrom(plain, theme), iconType: 'image' })).toEqual(plain)
        expect(applyFolderDraft(plain, { ...folderDraftFrom(plain, theme), iconType: 'image', imgFile: 'assets/new.png' })).toEqual({ ...plain, imgFile: 'assets/new.png' })
        const withImage = stored({ imgFile: 'assets/f.png', img: 'blob:f' })
        expect(applyFolderDraft(withImage, { ...folderDraftFrom(withImage, theme), imgFile: null })).toEqual(stored({ imgFile: null as unknown as string, img: '' }))
    })
})

describe('releaseFolder', () => {
    it('puts the characters where the folder was, in folder order', () => {
        const folder = stored({ data: ['c', 'a', 'b'] })
        const other = stored({ id: 'f2', data: ['d'] })
        expect(releaseFolder(['x', folder, 'y', other], 'f1')).toEqual(['x', 'c', 'a', 'b', 'y', other])
        const order = ['x', folder]
        expect(releaseFolder(order, 'missing')).toBe(order)
    })
})
