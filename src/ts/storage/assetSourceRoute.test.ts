import { describe, expect, it } from 'vitest'
import { selectAssetSourceRoute } from './assetSourceRoute'

describe('selectAssetSourceRoute', () => {
    it('selects the official account URL before the Tauri asset route', () => {
        expect(selectAssetSourceRoute('assets/synthetic.png', true, true)).toBe('account')
    })

    it('keeps non-account Tauri assets on the local BlobStore route', () => {
        expect(selectAssetSourceRoute('assets/synthetic.png', true, false)).toBe('tauri-asset')
    })

    it('keeps non-asset Tauri paths on convertFileSrc', () => {
        expect(selectAssetSourceRoute('local/file.txt', true, true)).toBe('tauri-path')
    })
})
