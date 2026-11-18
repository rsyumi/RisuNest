import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushSync, mount, tick, unmount } from 'svelte'

vi.mock('src/ts/stores.svelte', async () => (await import('src/ts/importIntake/openResult.testSupport')).importStoresModule())

import { changeLanguage, language } from 'src/lang'
import { fillTemplate } from 'src/ts/gui/nativeFileJobDialogModel'
import { importDropOverlayVisible, openImportSheet } from 'src/ts/importIntake/sheetStore'
import type { ImportIntakeItem, ImportSheetDecision, ImportSheetRequest } from 'src/ts/importIntake/types'
import ImportDropOverlay from './ImportDropOverlay.svelte'
import ImportIntakeSheet from './ImportIntakeSheet.svelte'

const intake = language.risuNest.importIntake
const dialog = language.risuNest.importDialog

function item(id: string, name: string, fields: Omit<ImportIntakeItem, 'id' | 'name' | 'source'>): ImportIntakeItem {
    return { id, name, source: { type: 'path', path: `C:/synthetic/${name}` }, ...fields }
}

const character = item('character', 'Mira.png', { kind: 'character', format: 'png', bytes: 1024 })
const charx = item('charx', 'Night.module.charx', { kind: 'character', format: 'charx', bytes: 2048, moduleHint: true })
const lorebook = item('lorebook', 'Harbor.json', { kind: 'lorebook', format: 'json', bytes: 10 })
const preset = item('preset', 'Calm.risup', { kind: 'preset', format: 'risup', bytes: 10 })
const plugin = item('plugin', 'synthetic.js', {
    kind: 'plugin', format: 'js', bytes: 3,
    plugin: { header: { name: 'synthetic', displayName: 'Synthetic Plugin', version: '1.2.0', apiVersion: '3.0' }, code: '//', isTypescript: false },
})
const oldPlugin = item('old', 'old.js', { kind: 'plugin', format: 'js', bytes: 5, refusal: { code: 'plugin-api-version', version: '2.1' } })
const notes = item('notes', 'notes.txt', { kind: 'unsupported', bytes: 10, refusal: { code: 'unsupported-format' } })
const backup = item('backup', 'library.risunest', { kind: 'backup', format: 'portable', bytes: 2048, refusal: { code: 'backup-with-other-files' } })
const selected = { chaId: 'synthetic-character', name: 'Mira' }

let component: ReturnType<typeof mount> | undefined
let target: HTMLDivElement

beforeEach(() => {
    target = document.createElement('div')
    document.body.append(target)
    component = mount(ImportIntakeSheet, { target })
})

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target.remove()
    importDropOverlayVisible.set(false)
})

function open(request: Partial<ImportSheetRequest> & Pick<ImportSheetRequest, 'items'>) {
    let decision: ImportSheetDecision | undefined
    const settled = openImportSheet({ defaults: {}, installPlugin: async () => false, ...request }).then(value => { decision = value })
    flushSync()
    return { settled, decided: () => decision }
}

const row = (id: string) => target.querySelector<HTMLElement>(`[data-import-row="${id}"]`)!
const rowText = (id: string) => row(id).textContent!.replace(/\s+/g, ' ').trim()
const include = (id: string) => row(id).querySelector<HTMLInputElement>('input[type="checkbox"]')
const footer = () => [...target.querySelectorAll('footer button')].map(button => button.textContent!.trim())
const footerButton = (label: string) => [...target.querySelectorAll<HTMLButtonElement>('footer button')].find(button => button.textContent!.trim() === label)!

function choose(id: string, value: 'character' | 'module') {
    row(id).querySelector<HTMLButtonElement>(`[role="radio"]:nth-child(${value === 'character' ? 1 : 2})`)!.click()
    flushSync()
}

function destination(id: string, value: 'character' | 'new-module') {
    const select = row(id).querySelector('select')!
    select.value = value
    select.dispatchEvent(new Event('change', { bubbles: true }))
    flushSync()
}

describe('import sheet', () => {
    it('lists every file in delivery order with what it is and what can be done with it', () => {
        open({ items: [character, charx, lorebook, preset, plugin, oldPlugin, notes, backup], lorebookCharacter: selected })

        const sheet = target.querySelector('[role="dialog"]')!
        expect(sheet.querySelector('header')!.textContent!.replace(/\s+/g, ' ').trim()).toBe(`${dialog.titleImport} ${fillTemplate(dialog.batchFiles, 8)}`)
        expect([...target.querySelectorAll('[data-import-row]')].map(element => element.getAttribute('data-import-row')))
            .toEqual(['character', 'charx', 'lorebook', 'preset', 'plugin', 'old', 'notes', 'backup'])
        expect(rowText('character')).toBe(`Mira.png ${language.character} · 1.0 KiB`)
        expect(rowText('charx')).toContain(`Night.module.charx ${intake.kindCharx} · 2.0 KiB`)
        expect(rowText('lorebook')).toContain(`Harbor.json ${language.loreBook} · 10 B`)
        expect(rowText('preset')).toBe(`Calm.risup ${language.presets} · 10 B`)
        expect(rowText('plugin')).toBe(`synthetic.js ${language.plugin} · Synthetic Plugin 1.2.0 · 3 B ${intake.install}`)
        expect(rowText('old')).toBe(`old.js 5 B · ${language.risuNest.plugins.unsupportedApiVersionInstall.replace('{version}', '2.1')}`)
        expect(rowText('notes')).toBe(`notes.txt 10 B · ${dialog.reasonUnsupportedFormat}`)
        expect(rowText('backup')).toBe(`library.risunest ${intake.kindBackupPortable} · 2.0 KiB · ${intake.reasonBackupWithOtherFiles}`)

        // Every importable row is checked to start with; plugins and refused files have no box.
        expect(['character', 'charx', 'lorebook', 'preset'].map(id => include(id)?.checked)).toEqual([true, true, true, true])
        expect(['plugin', 'old', 'notes', 'backup'].map(id => include(id))).toEqual([null, null, null, null])
        for (const id of ['old', 'notes', 'backup']) expect(row(id).querySelector('button, select, input')).toBeNull()

        const kind = row('charx').querySelector('[role="radiogroup"]')!
        expect(kind.getAttribute('aria-label')).toBe(fillTemplate(intake.charxAsLabel, 'Night.module.charx'))
        expect([...kind.querySelectorAll('[role="radio"]')].map(radio => [radio.textContent!.trim(), radio.getAttribute('aria-checked')]))
            .toEqual([[language.character, 'false'], [language.module, 'true']])
        const select = row('lorebook').querySelector('select')!
        expect(select.getAttribute('aria-label')).toBe(fillTemplate(intake.destinationLabel, 'Harbor.json'))
        expect([...select.options].map(option => option.textContent!.trim()))
            .toEqual([fillTemplate(intake.destinationCharacter, 'Mira'), intake.destinationNewModule])
        expect(select.value).toBe('character')
        expect(footer()).toEqual([language.cancel, intake.importAction])
    })

    it('imports the checked rows in delivery order with the chosen destinations', async () => {
        const { settled, decided } = open({
            items: [character, item('charx', 'Night.charx', { kind: 'character', format: 'charx' }), lorebook, plugin, notes],
            lorebookCharacter: selected,
        })
        include('character')!.click()
        flushSync()
        choose('charx', 'module')
        destination('lorebook', 'new-module')
        footerButton(intake.importAction).click()
        await settled
        expect(decided()).toEqual([{ id: 'charx', charxAs: 'module' }, { id: 'lorebook', lorebookTarget: { type: 'new-module' } }])
        expect(target.querySelector('[role="dialog"]')).toBeNull()
    })

    it('starts from the defaults and imports them unchanged', async () => {
        const { settled, decided } = open({
            items: [charx, lorebook],
            lorebookCharacter: selected,
            defaults: { charx: { charxAs: 'character' }, lorebook: { lorebookTarget: { type: 'new-module' } } },
        })
        expect(row('lorebook').querySelector('select')!.value).toBe('new-module')
        footerButton(intake.importAction).click()
        await settled
        expect(decided()).toEqual([
            { id: 'charx', charxAs: 'character' },
            { id: 'lorebook', lorebookTarget: { type: 'new-module' } },
        ])
    })

    it('offers the selected character for a single lorebook and only a new module without one', async () => {
        const withCharacter = open({ items: [lorebook], lorebookCharacter: selected })
        expect(include('lorebook')).toBeNull()
        footerButton(intake.importAction).click()
        await withCharacter.settled
        expect(withCharacter.decided()).toEqual([{ id: 'lorebook', lorebookTarget: { type: 'character', ...selected } }])

        const without = open({ items: [lorebook], defaults: { lorebook: { lorebookTarget: { type: 'character', ...selected } } } })
        const select = row('lorebook').querySelector('select')!
        expect([...select.options].map(option => option.value)).toEqual(['new-module'])
        expect(select.value).toBe('new-module')
        footerButton(intake.importAction).click()
        await without.settled
        expect(without.decided()).toEqual([{ id: 'lorebook', lorebookTarget: { type: 'new-module' } }])
    })

    it('disables importing once every row is unchecked', () => {
        open({ items: [character, preset] })
        include('character')!.click()
        include('preset')!.click()
        flushSync()
        expect(footerButton(intake.importAction).disabled).toBe(true)
        include('preset')!.click()
        flushSync()
        expect(footerButton(intake.importAction).disabled).toBe(false)
    })

    it('starts the next sheet from its own defaults', async () => {
        const first = open({ items: [character, charx, lorebook], lorebookCharacter: selected })
        include('character')!.click()
        flushSync()
        choose('charx', 'character')
        destination('lorebook', 'new-module')
        const second = open({ items: [character, charx, lorebook], lorebookCharacter: selected })
        await first.settled
        expect(first.decided()).toBeNull()
        expect(include('character')!.checked).toBe(true)
        footerButton(intake.importAction).click()
        await second.settled
        expect(second.decided()).toEqual([
            { id: 'character' },
            { id: 'charx', charxAs: 'module' },
            { id: 'lorebook', lorebookTarget: { type: 'character', ...selected } },
        ])
    })

    it.each([
        ['cancel', () => footerButton(language.cancel).click()],
        ['Escape', () => window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))],
        ['backdrop', () => target.querySelector<HTMLButtonElement>(`button[aria-label="${language.cancel}"]`)!.click()],
    ])('closes without importing on %s', async (_, close) => {
        const { settled, decided } = open({ items: [character, preset] })
        await tick()
        close()
        await settled
        expect(decided()).toBeNull()
        expect(target.querySelector('[role="dialog"]')).toBeNull()
    })

    it('offers only close when nothing can be imported', async () => {
        const { settled, decided } = open({ items: [plugin, notes] })
        expect(footer()).toEqual([dialog.close])
        footerButton(dialog.close).click()
        await settled
        expect(decided()).toBeNull()
    })

    it('uses the language chosen after the app started', () => {
        changeLanguage('ko')
        try {
            expect(language.risuNest.importIntake.importAction).not.toBe(intake.importAction)
            open({ items: [character, plugin, lorebook], lorebookCharacter: selected })
            expect(footer()).toEqual([language.cancel, language.risuNest.importIntake.importAction])
            expect(row('plugin').querySelector('button')!.textContent!.trim()).toBe(language.risuNest.importIntake.install)
            expect(row('lorebook').querySelector('select')!.getAttribute('aria-label'))
                .toBe(fillTemplate(language.risuNest.importIntake.destinationLabel, 'Harbor.json'))
        } finally {
            changeLanguage('en')
        }
    })

    it('installs a plugin from its row, holds the sheet while installing, and leaves it out of the import', async () => {
        let finish: (installed: boolean) => void = () => {}
        const installPlugin = vi.fn((_: ImportIntakeItem) => new Promise<boolean>(resolve => { finish = resolve }))
        const { settled, decided } = open({ items: [plugin, character], installPlugin })
        await tick()
        const install = () => row('plugin').querySelector('button')

        expect(install()!.textContent!.trim()).toBe(intake.install)
        install()!.click()
        expect(installPlugin).toHaveBeenCalledWith(plugin)
        flushSync()
        expect(install()!.disabled).toBe(true)
        // Escape stays with the plugin dialog above the sheet.
        const escape = new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true })
        window.dispatchEvent(escape)
        expect(escape.defaultPrevented).toBe(false)
        footerButton(language.cancel).click()
        footerButton(intake.importAction).click()
        await tick()
        expect(decided()).toBeUndefined()

        finish(false)
        await vi.waitFor(() => expect(install()!.disabled).toBe(false))
        install()!.click()
        finish(true)
        await vi.waitFor(() => expect(install()).toBeNull())
        expect(rowText('plugin')).toBe(`synthetic.js ${language.plugin} · Synthetic Plugin 1.2.0 · 3 B ${intake.installed}`)

        footerButton(intake.importAction).click()
        await settled
        expect(decided()).toEqual([{ id: 'character' }])
    })

    it('does not carry a pending install into the next sheet', async () => {
        let finish: (installed: boolean) => void = () => {}
        open({ items: [plugin, character], installPlugin: () => new Promise<boolean>(resolve => { finish = resolve }) })
        row('plugin').querySelector('button')!.click()
        flushSync()
        open({ items: [plugin, character] })
        finish(true)
        await tick()
        await tick()
        const install = row('plugin').querySelector('button')!
        expect([install.textContent!.trim(), install.disabled]).toEqual([intake.install, false])
    })
})

describe('drop overlay', () => {
    it('shows the drop hint only while a file drag is over the window', () => {
        const overlay = document.createElement('div')
        document.body.append(overlay)
        const mounted = mount(ImportDropOverlay, { target: overlay })
        try {
            expect(overlay.textContent!.trim()).toBe('')
            importDropOverlayVisible.set(true)
            flushSync()
            expect(overlay.textContent!.trim()).toBe(intake.dropHint)
            importDropOverlayVisible.set(false)
            flushSync()
            expect(overlay.textContent!.trim()).toBe('')
        } finally {
            void unmount(mounted)
            overlay.remove()
        }
    })
})
