// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { createRawSnippet, tick, type ComponentProps } from 'svelte'
import { createClassComponent } from 'svelte/legacy'

import SidebarAvatar from './SidebarAvatar.svelte'

vi.mock('src/ts/gui/tooltip', () => ({
    tooltipRight: () => ({}),
}))

class TestIntersectionObserver {
    static instances: TestIntersectionObserver[] = []

    readonly observed = new Set<Element>()
    readonly unobserve = vi.fn((element: Element) =>
        this.observed.delete(element),
    )
    readonly disconnect = vi.fn(() => this.observed.clear())

    constructor(private readonly callback: IntersectionObserverCallback) {
        TestIntersectionObserver.instances.push(this)
    }

    observe(element: Element) {
        this.observed.add(element)
    }

    setVisible(...elements: Element[]) {
        this.callback(
            elements.map(
                (target) =>
                    ({
                        target,
                        isIntersecting: true,
                    }) as IntersectionObserverEntry,
            ),
            this as unknown as IntersectionObserver,
        )
    }
}

type AvatarComponent = ReturnType<typeof createClassComponent>
type AvatarProps = ComponentProps<typeof SidebarAvatar>
type RenderAvatarProps = Pick<AvatarProps, 'src'> &
    Partial<Omit<AvatarProps, 'src'>>
const mounted: AvatarComponent[] = []

function deferred<T>() {
    let reject!: (reason?: unknown) => void
    const promise = new Promise<T>((_resolve, rejectPromise) => {
        reject = rejectPromise
    })
    return { promise, reject }
}

function renderAvatar(props: RenderAvatarProps) {
    const target = document.createElement('div')
    document.body.appendChild(target)
    const component = createClassComponent({
        component: SidebarAvatar,
        target,
        props: {
            rounded: false,
            name: 'Synthetic avatar',
            ...props,
        },
    })
    mounted.push(component)
    return { component, target }
}

afterEach(() => {
    while (mounted.length > 0) mounted.pop()?.$destroy()
    TestIntersectionObserver.instances = []
    vi.unstubAllGlobals()
    document.body.replaceChildren()
})

describe('SidebarAvatar lazy sources', () => {
    it('renders the avatar placeholder when an existing source promise rejects', async () => {
        const source = deferred<string>()
        const { target } = renderAvatar({ src: source.promise, rounded: true })

        source.reject(new Error('synthetic avatar failure'))
        await tick()

        expect(target.querySelector('img')).toBeNull()
        expect(target.querySelector('.sidebar-avatar')).not.toBeNull()
    })

    it('renders the slot placeholder when an existing background promise rejects', async () => {
        const background = deferred<string>()
        const { target } = renderAvatar({
            src: 'slot',
            backgroundimg: background.promise,
        })

        background.reject(new Error('synthetic background failure'))
        await tick()

        const placeholder = target.querySelector<HTMLElement>('.sidebar-avatar')
        expect(placeholder).not.toBeNull()
        expect(placeholder?.style.backgroundImage).toBe('')
    })

    it('shares one observer across hidden avatars and evaluates only visible callbacks', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const sources = Array.from({ length: 100 }, (_, index) =>
            vi.fn(() => `/avatar-${index}.png`),
        )
        const targets = sources.map(
            (src, index) =>
                renderAvatar({
                    src,
                    chaId: `character-${index}`,
                }).target,
        )
        await tick()

        expect(TestIntersectionObserver.instances).toHaveLength(1)
        expect(sources.every((source) => source.mock.calls.length === 0)).toBe(
            true,
        )

        const first = targets[0].querySelector('[data-char-id="character-0"]')!
        const last = targets[99].querySelector('[data-char-id="character-99"]')!
        TestIntersectionObserver.instances[0].setVisible(first, last)
        await tick()

        expect(sources[0]).toHaveBeenCalledTimes(1)
        expect(sources[99]).toHaveBeenCalledTimes(1)
        expect(
            sources
                .slice(1, 99)
                .every((source) => source.mock.calls.length === 0),
        ).toBe(true)
        expect(targets[0].querySelector('img')?.getAttribute('src')).toBe(
            '/avatar-0.png',
        )
        expect(targets[99].querySelector('img')?.getAttribute('src')).toBe(
            '/avatar-99.png',
        )
    })

    it('uses the latest source before visibility and fences stale async results after visibility', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        let resolveBefore: (value: string) => void = () => {}
        let resolveLatest: (value: string) => void = () => {}
        const initial = vi.fn(() => '/initial.png')
        const beforeVisible = vi.fn(
            () =>
                new Promise<string>((resolve) => {
                    resolveBefore = resolve
                }),
        )
        const latest = vi.fn(
            () =>
                new Promise<string>((resolve) => {
                    resolveLatest = resolve
                }),
        )
        const { component, target } = renderAvatar({ src: initial })
        await tick()

        component.$set({ src: beforeVisible })
        await tick()
        const avatar = target.querySelector('.avatar')!
        TestIntersectionObserver.instances[0].setVisible(avatar)
        await tick()

        expect(initial).not.toHaveBeenCalled()
        expect(beforeVisible).toHaveBeenCalledTimes(1)

        component.$set({ src: latest })
        await tick()
        expect(latest).toHaveBeenCalledTimes(1)

        resolveBefore('/stale.png')
        await tick()
        expect(target.querySelector('img')?.getAttribute('src')).not.toBe(
            '/stale.png',
        )

        resolveLatest('/latest.png')
        await tick()
        expect(target.querySelector('img')?.getAttribute('src')).toBe(
            '/latest.png',
        )
    })

    it('loads character and folder images again after the settings screen remounts the sidebar', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        const characterSource = vi.fn(() => Promise.resolve('/character.png'))
        const folderSource = vi.fn(() => Promise.resolve('/folder.png'))
        const firstCharacter = renderAvatar({ src: characterSource })
        const hiddenCharacter = renderAvatar({ src: () => '/hidden.png' })
        await tick()

        const firstObserver = TestIntersectionObserver.instances[0]
        firstObserver.setVisible(
            firstCharacter.target.querySelector('.avatar')!,
        )
        await tick()
        expect(
            firstCharacter.target.querySelector('img')?.getAttribute('src'),
        ).toBe('/character.png')

        firstCharacter.component.$destroy()
        hiddenCharacter.component.$destroy()
        mounted.splice(0, 2)
        expect(firstObserver.observed.size).toBe(0)

        const remountedCharacter = renderAvatar({ src: characterSource })
        const remountedFolder = renderAvatar({
            src: 'slot',
            backgroundimg: folderSource,
        })
        await tick()
        const nextObserver = TestIntersectionObserver.instances.at(-1)!
        expect(nextObserver).not.toBe(firstObserver)
        nextObserver.setVisible(
            remountedCharacter.target.querySelector('.avatar')!,
            remountedFolder.target.querySelector('.avatar')!,
        )
        await tick()

        expect(characterSource).toHaveBeenCalledTimes(2)
        expect(folderSource).toHaveBeenCalledTimes(1)
        expect(
            remountedCharacter.target.querySelector('img')?.getAttribute('src'),
        ).toBe('/character.png')
        expect(
            remountedFolder.target.querySelector<HTMLElement>('.sidebar-avatar')
                ?.style.backgroundImage,
        ).toContain('/folder.png')
    })

    it('loads immediately without IntersectionObserver and keeps fallback content on rejection', async () => {
        vi.stubGlobal('IntersectionObserver', undefined)
        const normalSource = vi.fn(() =>
            Promise.reject(new Error('synthetic image failure')),
        )
        const backgroundSource = vi.fn(() =>
            Promise.reject(new Error('synthetic background failure')),
        )
        const normal = renderAvatar({ src: normalSource })
        const folder = renderAvatar({
            src: 'slot',
            backgroundimg: backgroundSource,
            children: createRawSnippet(() => ({
                render: () => '<span data-folder-fallback>folder</span>',
            })),
        })
        await tick()
        await Promise.resolve()
        await tick()

        expect(normalSource).toHaveBeenCalledTimes(1)
        expect(backgroundSource).toHaveBeenCalledTimes(1)
        expect(normal.target.querySelector('img')).toBeNull()
        expect(normal.target.querySelector('.sidebar-avatar')).not.toBeNull()
        expect(
            folder.target.querySelector('[data-folder-fallback]')?.textContent,
        ).toBe('folder')
    })
})
