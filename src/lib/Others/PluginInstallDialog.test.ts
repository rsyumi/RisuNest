import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'

const h = vi.hoisted(() => ({ importPlugin: vi.fn<typeof import('src/ts/plugins/plugins.svelte')['importPlugin']>() }))
vi.mock('src/ts/plugins/plugins.svelte', () => ({ importPlugin: h.importPlugin }) satisfies Partial<typeof import('src/ts/plugins/plugins.svelte')>)
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())

import { language } from 'src/lang'
import { alertStore, DBState } from 'src/ts/stores.svelte'
import { fillTemplate, formatBytes } from 'src/ts/gui/nativeFileJobDialogModel'
import { inspectPluginFile, pluginInstallDialog, requestPluginInstall } from 'src/ts/importIntake/pluginInstall'
import type { ImportIntakeItem } from 'src/ts/importIntake/types'
import type { RisuPlugin } from 'src/ts/plugins/plugins.svelte'
import PluginInstallDialog from './PluginInstallDialog.svelte'

const copy = language.risuNest.importIntake
const declared = [
    '//@name synthetic',
    '//@display-name Synthetic Plugin',
    '//@api 3.0',
    '//@version 1.2.0',
    '//@update-url https://plugins.example.test/path/synthetic.js',
    '',
    'console.log("synthetic")',
].join('\n')

let component: ReturnType<typeof mount> | undefined
let target: HTMLDivElement

function pluginItem(name: string, code: string, bytes?: number): ImportIntakeItem {
    const inspected = inspectPluginFile(name, code, false)
    if (!('header' in inspected)) throw new Error(`refused: ${inspected.refusal.code}`)
    return { id: name, name, bytes, source: { type: 'path', path: name }, kind: 'plugin', format: 'js',
        plugin: { header: inspected.header, code, isTypescript: false } }
}

function dialog(): HTMLElement | null {
    return target.querySelector('[role="dialog"]')
}

function rows(): [string, string][] {
    return [...dialog()!.querySelectorAll('dt')].map(term => [
        term.textContent!.trim(),
        term.nextElementSibling!.textContent!.replace(/\s+/g, ' ').trim(),
    ])
}

function button(label: string): HTMLButtonElement {
    const found = [...dialog()!.querySelectorAll('button')].find(element => element.textContent?.trim() === label)
    if (!found) throw new Error(`no ${label} button`)
    return found
}

function installedPlugin(fields: Partial<RisuPlugin>): RisuPlugin {
    return { name: 'synthetic', script: '', realArg: {}, arguments: {}, version: '3.0', customLink: [], argMeta: {}, enabled: true, ...fields }
}

beforeEach(() => {
    DBState.db = { plugins: [] } as unknown as typeof DBState.db
    target = document.createElement('div')
    document.body.append(target)
    component = mount(PluginInstallDialog, { target })
})

afterEach(async () => {
    for (let left = 10; get(pluginInstallDialog) && left > 0; left--) get(pluginInstallDialog)!.cancel()
    if (component) await unmount(component)
    component = undefined
    target.remove()
    alertStore.set({ type: 'none', msg: '' })
    h.importPlugin.mockReset()
})

describe('plugin file inspection', () => {
    it.each([
        ['console.log("no header")', { code: 'unsupported-format' }],
        ['//@name legacy\n//@api 2.1', { code: 'plugin-api-version', version: '2.1' }],
        ['//@name bannerless', { code: 'plugin-api-version', version: '2.0' }],
    ])('refuses %j', (code, refusal) => {
        expect(inspectPluginFile('synthetic.js', code, false)).toEqual({ refusal })
    })

    it('reads a header behind a UTF-8 BOM and keeps only the update host', () => {
        expect(inspectPluginFile('synthetic.js', `﻿${declared}`, false)).toEqual({ header: {
            name: 'synthetic', displayName: 'Synthetic Plugin', version: '1.2.0', apiVersion: '3.0', updateHost: 'plugins.example.test',
        } })
    })
})

describe('plugin install dialog', () => {
    it('offers a new plugin with its declared fields and installs it once without asking again', async () => {
        let finishInstall!: (installed: boolean) => void
        h.importPlugin.mockImplementation(() => new Promise(resolve => { finishInstall = resolve }))
        const result = requestPluginInstall(pluginItem('synthetic.js', `﻿${declared}`, 2048))
        await tick()

        expect(dialog()!.querySelector('h2')!.textContent).toBe(copy.pluginInstallTitle)
        expect(rows()).toEqual([
            [language.name, 'Synthetic Plugin synthetic'],
            [copy.pluginVersion, '1.2.0'],
            [copy.pluginUpdateURL, 'plugins.example.test'],
            [copy.detailFile, `synthetic.js · ${formatBytes(2048)}`],
        ])
        expect(dialog()!.textContent).toContain(`${copy.pluginTrustedSources} ${language.pluginWarn}`)
        expect([...dialog()!.querySelectorAll('button')].map(element => element.textContent?.trim())).toEqual([language.cancel, copy.install])

        button(copy.install).click()
        button(copy.install).click()
        await tick()
        expect(button(language.cancel).disabled).toBe(true)
        window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
        await tick()
        expect(dialog()).not.toBeNull()

        finishInstall(true)
        expect(await result).toBe('installed')
        expect(h.importPlugin).toHaveBeenCalledExactlyOnceWith(declared, { isTypescript: false, confirmed: true })
        await tick()
        expect(dialog()).toBeNull()
    })

    it('asks to replace an installed plugin with both versions and reports a failed install', async () => {
        DBState.db.plugins = [installedPlugin({ displayName: 'Installed Synthetic', versionOfPlugin: '1.1.0' })]
        h.importPlugin.mockResolvedValue(false)
        const result = requestPluginInstall(pluginItem('synthetic.js', declared, 2048))
        await tick()

        expect(dialog()!.querySelector('h2')!.textContent).toBe(copy.pluginReplaceTitle)
        expect(rows().slice(1, 3)).toEqual([
            [copy.pluginInstalledVersion, '1.1.0'],
            [copy.pluginNewVersion, '1.2.0'],
        ])
        expect(dialog()!.textContent).toContain(fillTemplate(copy.pluginReplaceData, 'Installed Synthetic'))
        button(copy.replace).click()
        expect(await result).toBe('failed')
    })

    it('leaves out the rows a header does not declare', async () => {
        DBState.db.plugins = [installedPlugin({ versionOfPlugin: '' })]
        void requestPluginInstall(pluginItem('synthetic.js', '//@name synthetic\n//@api 3.0'))
        await tick()
        expect(rows()).toEqual([[language.name, 'synthetic'], [copy.detailFile, 'synthetic.js']])
        expect(dialog()!.textContent).toContain(fillTemplate(copy.pluginReplaceData, 'synthetic'))
    })

    it.each([
        ['the cancel button', () => button(language.cancel).click()],
        ['Escape', () => dialog()!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))],
        ['a backdrop click', () => (dialog()!.parentElement as HTMLElement).click()],
    ])('installs nothing after %s and leaves a sheet below open', async (_, dismiss) => {
        const sheetKeydown = vi.fn()
        window.addEventListener('keydown', sheetKeydown)
        const result = requestPluginInstall(pluginItem('synthetic.js', declared))
        await tick()
        dialog()!.querySelector('dl')!.click()
        await tick()
        expect(dialog()).not.toBeNull()

        dismiss()
        expect(await result).toBe('cancelled')
        window.removeEventListener('keydown', sheetKeydown)
        expect(sheetKeydown).not.toHaveBeenCalled()
        expect(h.importPlugin).not.toHaveBeenCalled()
        await tick()
        expect(dialog()).toBeNull()
    })

    it('leaves Escape to an alert shown above it', async () => {
        void requestPluginInstall(pluginItem('synthetic.js', declared))
        await tick()
        alertStore.set({ type: 'normal', msg: 'synthetic alert' })
        dialog()!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
        await tick()
        expect(dialog()).not.toBeNull()
    })

    it('shows requests one at a time and treats a name the earlier one installed as a replacement', async () => {
        h.importPlugin.mockImplementation(async code => {
            DBState.db.plugins.push(installedPlugin({ script: code!, versionOfPlugin: '1.2.0' }))
            return true
        })
        const first = requestPluginInstall(pluginItem('first.js', declared))
        const second = requestPluginInstall(pluginItem('second.js', declared.replace('1.2.0', '1.3.0')))
        await tick()
        expect(target.querySelectorAll('[role="dialog"]')).toHaveLength(1)
        expect(rows().at(-1)).toEqual([copy.detailFile, 'first.js'])

        button(copy.install).click()
        expect(await first).toBe('installed')
        await tick()
        expect(dialog()!.querySelector('h2')!.textContent).toBe(copy.pluginReplaceTitle)
        expect(rows().at(-1)).toEqual([copy.detailFile, 'second.js'])
        expect(rows()).toContainEqual([copy.pluginInstalledVersion, '1.2.0'])
        button(language.cancel).click()
        expect(await second).toBe('cancelled')
    })
})
