import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { languageEnglish } from '../../lang/en'
const mocks = vi.hoisted(() => ({ horde: vi.fn(async () => []), changed: vi.fn() }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: { customModels: [] } } }))
vi.mock('src/lang', () => ({ language: languageEnglish }))
vi.mock('src/ts/alert', () => ({ alertMd: vi.fn() }))
vi.mock('src/ts/setting/utils', () => ({ resolveLanguagePath: () => '' }))
vi.mock('src/ts/horde/getModels', () => ({ getHordeModels: mocks.horde }))
vi.mock('src/ts/model/modellist', () => ({
    getModelInfo: (id: string) => ({ fullName: id }),
    getModelList: () => [
        { providerName: '@as-is', models: [{ id: 'base', name: 'Base model' }] },
        {
            providerName: 'Plugins',
            models: [
                { id: 'pluginmodel:::one', name: 'Plugin one' },
                { id: 'pluginmodel:::two', name: 'Plugin two' },
            ],
        },
    ],
}))
import ModelList from './ModelList.svelte'
let instance: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (instance) await unmount(instance)
    document.body.replaceChildren()
    vi.clearAllMocks()
})
it('opens a selected plugin on its own tab, filters it and preserves blank selection callback', async () => {
    instance = mount(ModelList, {
        target: document.body,
        props: {
            value: 'pluginmodel:::one',
            excludesPrefix: 'pluginmodel:::two',
            blankable: true,
            onChange: mocks.changed,
        },
    })
    document.querySelector('button')!.click()
    await tick()
    expect(document.querySelector('[role=tab][aria-selected=true]')?.textContent).toBe(languageEnglish.plugin)
    expect(document.body.textContent).toContain('Plugin one')
    expect(document.body.textContent).not.toContain('Plugin two')
    expect(document.body.textContent).not.toContain('Base model')
    expect(mocks.horde).not.toHaveBeenCalled()
    const none = [...document.querySelectorAll('button')].find(
        (button) => button.textContent === languageEnglish.none,
    )!
    none.click()
    await tick()
    expect(mocks.changed).toHaveBeenCalledWith('')
})
it('shows a missing selected plugin without clearing its value', async () => {
    instance = mount(ModelList, {
        target: document.body,
        props: { value: 'pluginmodel:::missing', onChange: mocks.changed },
    })
    document.querySelector('button')!.click()
    await tick()
    expect(document.body.textContent).toContain(languageEnglish.pluginModelUnavailable)
    expect(mocks.changed).not.toHaveBeenCalled()
})
