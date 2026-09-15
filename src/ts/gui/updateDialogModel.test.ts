import { describe, expect, it } from 'vitest'
import { buildUpdateDialogModel } from './updateDialogModel'
import { initialAppUpdateState } from '../update/state.svelte'

describe('update dialog model', () => {
    it('shows determinate download progress and permits cancellation only while downloading', () => {
        const downloading = buildUpdateDialogModel({
            ...initialAppUpdateState,
            phase: 'downloading',
            popupVisible: true,
            progress: { handleId: 'h', downloaded: 25, total: 100 },
        })
        expect(downloading).toMatchObject({ open: true, busy: true, canCancel: true, percent: 25 })
        const applying = buildUpdateDialogModel({ ...initialAppUpdateState, phase: 'applying', popupVisible: true })
        expect(applying).toMatchObject({ busy: true, canCancel: false })
    })
})
