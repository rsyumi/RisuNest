import {afterEach, beforeEach, describe, expect, it, vi} from 'vitest'
const dialog = vi.hoisted(() => vi.fn())
const actionDialog = vi.hoisted(() => vi.fn())
const characters = vi.hoisted(() => vi.fn())
vi.mock('../alert', () => ({alertCheckboxConfirm:dialog, alertActionConfirm:actionDialog}))
vi.mock('./libraryCharacters', () => ({hasLibraryCharacters:characters}))
import {confirmUpstreamLibraryReplacement} from './upstreamReplacement'
import {language} from '../../lang'

const details = [{label:language.risuNest.importIntake.detailFile,value:'synthetic.risunest · 1.0 KiB'},{label:language.risuNest.importIntake.detailKind,value:language.risuNest.importIntake.kindBackupPortable}]

beforeEach(() => {
    characters.mockReset().mockResolvedValue(true)
})
afterEach(() => {
    dialog.mockReset()
    actionDialog.mockReset()
})

describe('upstream import acknowledgement', () => {
    it.each([false,true])('requires a checked acknowledgement and includes every warning, bound=%s', async bound => {
        dialog.mockResolvedValueOnce({confirmed:true,checked:true})
        await expect(confirmUpstreamLibraryReplacement(bound,['cold warning','inlay warning'],{details})).resolves.toBe(true)
        expect(dialog).toHaveBeenLastCalledWith(expect.objectContaining({
            details,
            description:[bound ? language.lwwSync.restoreDescriptionBound : language.lwwSync.restoreDescription,'cold warning','inlay warning'].join('\n\n'),
            checkboxLabel:language.lwwSync.restoreAcknowledge,requireChecked:true,
        }))
    })
    it('declines cancellation before activation', async () => {
        dialog.mockResolvedValueOnce({confirmed:false,checked:false})
        await expect(confirmUpstreamLibraryReplacement(true)).resolves.toBe(false)
    })
    it.each([false,true])('restores without asking when the library holds no character, bound=%s', async bound => {
        characters.mockResolvedValue(false)
        await expect(confirmUpstreamLibraryReplacement(bound,[],{details})).resolves.toBe(true)
        expect(characters).toHaveBeenCalledOnce()
        expect(dialog).not.toHaveBeenCalled()
        expect(actionDialog).not.toHaveBeenCalled()
    })
    it.each([[false,true],[true,false]])('shows only the backup warnings when the library holds no character, bound=%s answer=%s', async (bound, answer) => {
        characters.mockResolvedValue(false)
        actionDialog.mockResolvedValueOnce(answer)
        await expect(confirmUpstreamLibraryReplacement(bound,['cold warning','inlay warning'],{details})).resolves.toBe(answer)
        expect(dialog).not.toHaveBeenCalled()
        expect(actionDialog).toHaveBeenCalledExactlyOnceWith({
            details,
            title:language.lwwSync.restoreTitle, description:['cold warning','inlay warning'].join('\n\n'),
            actionLabel:language.lwwSync.restoreAction, cancelLabel:language.lwwSync.cancelAction,
        })
    })
    it('keeps first-run consent when the library holds a character', async () => {
        await expect(confirmUpstreamLibraryReplacement(false, [], {firstRun:true})).resolves.toBe(true)
        expect(characters).not.toHaveBeenCalled()
        expect(dialog).not.toHaveBeenCalled()
        expect(actionDialog).not.toHaveBeenCalled()
    })
    it.each([true, false])('keeps first-run backup warnings without the overwrite checkbox, answer=%s', async answer => {
        actionDialog.mockResolvedValueOnce(answer)
        await expect(confirmUpstreamLibraryReplacement(false, ['inlay warning'], {firstRun:true})).resolves.toBe(answer)
        expect(actionDialog).toHaveBeenCalledExactlyOnceWith({
            title:language.lwwSync.restoreTitle, description:'inlay warning',
            actionLabel:language.lwwSync.restoreAction, cancelLabel:language.lwwSync.cancelAction,
        })
        expect(dialog).not.toHaveBeenCalled()
        expect(characters).not.toHaveBeenCalled()
    })
    it('still asks before a first-run import replaces a bound library that holds a character', async () => {
        dialog.mockResolvedValueOnce({confirmed:false,checked:false})
        await expect(confirmUpstreamLibraryReplacement(true, [], {firstRun:true})).resolves.toBe(false)
        expect(dialog).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({
            description:language.lwwSync.restoreDescriptionBound, requireChecked:true,
        }))
        expect(characters).toHaveBeenCalledOnce()
    })
})
