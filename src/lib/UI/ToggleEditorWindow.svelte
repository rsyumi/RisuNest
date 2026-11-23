<script lang="ts" module>
    import { getContext } from 'svelte'

    const windowKey = Symbol('toggleEditorWindow')

    export interface ToggleEditorWindowState {
        /** Whether the window is wide enough to show the preview beside the rows. */
        readonly wide: boolean
    }

    /** The pop-out window the calling component is drawn in, if any. */
    export function toggleEditorWindow(): ToggleEditorWindowState | undefined {
        return getContext<ToggleEditorWindowState | undefined>(windowKey)
    }
</script>

<script lang="ts">
    import { onMount, setContext, type Snippet } from 'svelte'
    import { language } from 'src/lang'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'

    interface Props {
        title: string
        onClose: () => void
        children: Snippet
    }

    let { title, onClose, children }: Props = $props()

    const titleId = $props.id()
    let width = $state(0)
    let viewportTop = $state(0)
    let viewportHeight: number | null = $state(null)

    setContext<ToggleEditorWindowState>(windowKey, {
        get wide() {
            return width >= 896
        },
    })

    onMount(() => {
        // Follow the visual viewport so an on-screen keyboard shrinks the window instead of covering it.
        const viewport = window.visualViewport
        if (!viewport) return
        const update = () => {
            viewportTop = viewport.offsetTop
            viewportHeight = viewport.height
        }
        update()
        viewport.addEventListener('resize', update)
        viewport.addEventListener('scroll', update)
        return () => {
            viewport.removeEventListener('resize', update)
            viewport.removeEventListener('scroll', update)
        }
    })
</script>

<!-- App hotkeys listen on the document and would act on the screen behind the window. -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
    class="fixed inset-x-0 z-modal flex bg-black/60 outline-hidden sm:items-center sm:justify-center sm:p-6 lg:p-10"
    style:top="{viewportTop}px"
    style:height={viewportHeight === null ? '100%' : `${viewportHeight}px`}
    tabindex="-1"
    role="presentation"
    onkeydown={(event) => event.stopPropagation()}
>
    <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        use:modalNavigation={{ close: onClose }}
        bind:clientWidth={width}
        class="flex h-full w-full flex-col bg-bgcolor text-textcolor sm:max-w-[1100px] sm:rounded-xl sm:border sm:border-darkborderc sm:shadow-2xl"
    >
        <div class="flex shrink-0 items-center justify-between gap-3 border-b border-darkborderc px-4 pt-[max(0.75rem,env(safe-area-inset-top))] pb-3 sm:pt-3">
            <h2 id={titleId} class="min-w-0 truncate text-lg font-medium">{title}</h2>
            <button
                type="button"
                class="bg-darkbutton hover:bg-selected focus:ring-selected border-darkborderc border text-textcolor rounded-md shadow-xs focus:outline-hidden focus:ring-2 transition-colors duration-200 px-4 py-2 text-sm shrink-0"
                onclick={onClose}
            >{language.close}</button>
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto px-4 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))]">
            {@render children()}
        </div>
    </div>
</div>
