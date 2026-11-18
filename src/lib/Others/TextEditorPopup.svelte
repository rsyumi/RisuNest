<script lang="ts">
    import { onMount, tick, untrack } from 'svelte'
    import { XIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { isMobile } from 'src/ts/platform'
    import { isCompositionKey } from 'src/ts/hotkeyModifier'
    import { modalNavigation } from 'src/ts/ui/modalNavigation'
    import {
        cancelTextEditorPopup,
        closeTextEditorPopup,
        markTextEditorPopupContent,
        type TextEditorPopupRequest,
    } from 'src/ts/gui/textEditorPopup.svelte'
    import type TextEditorMonacoComponent from './TextEditorMonaco.svelte'
    import type TextEditorPreviewComponent from './TextEditorPreview.svelte'
    import SettingButton from '../Setting/RisuNest/SettingButton.svelte'

    interface Props {
        request: TextEditorPopupRequest
    }

    let { request }: Props = $props()

    const strings = language.risuNest.textEditor
    let draft = $state(untrack(() => request.value))
    let saving = $state(false)
    let textarea: HTMLTextAreaElement | undefined = $state()
    let viewportTop = $state(0)
    let viewportHeight: number | null = $state(null)
    // Monaco does not support touch input, so phones and tablets keep the plain editor.
    let editorMode: 'loading' | 'monaco' | 'plain' = $state(isMobile ? 'plain' : 'loading')
    let Monaco: typeof TextEditorMonacoComponent | null = $state.raw(null)
    let monaco: { focus(): void } | undefined = $state()
    let monacoHost: HTMLDivElement | undefined = $state()
    let Preview: typeof TextEditorPreviewComponent | null = $state.raw(null)
    let previewing = $state(false)

    markTextEditorPopupContent()

    async function save() {
        if (saving) return
        saving = true
        try {
            if (await request.save(draft)) closeTextEditorPopup(request)
        } finally {
            saving = false
        }
    }

    function cancel() {
        if (saving) return
        cancelTextEditorPopup(request)
    }

    function focusEditor() {
        if (editorMode === 'monaco') monaco?.focus()
        else textarea?.focus({ preventScroll: true })
    }

    async function togglePreview() {
        if (previewing) {
            previewing = false
            await tick()
            // Like on opening, touch devices leave the reader to pick where to edit.
            if (!isMobile) focusEditor()
            return
        }
        Preview ??= await import('./TextEditorPreview.svelte').then((module) => module.default, () => null)
        if (Preview) previewing = true
    }

    function leaveEscape(event: KeyboardEvent) {
        return editorMode === 'monaco' && !previewing && event.target instanceof Node && !!monacoHost?.contains(event.target)
    }

    function handleKeydown(event: KeyboardEvent) {
        // App hotkeys listen on the document and would act on the screen behind the editor.
        // modalNavigation handles Escape and Back unless the code editor gets Escape first.
        event.stopPropagation()
        if (isCompositionKey(event)) return
        if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
            event.preventDefault()
            void save()
        } else if (event.key === 'Escape' && !event.defaultPrevented) {
            event.preventDefault()
            cancel()
        }
    }

    onMount(() => {
        let mounted = true
        if (editorMode === 'loading') {
            import('./TextEditorMonaco.svelte').then(
                (module) => {
                    if (!mounted) return
                    Monaco = module.default
                    editorMode = 'monaco'
                },
                async () => {
                    if (!mounted) return
                    editorMode = 'plain'
                    await tick()
                    textarea?.focus({ preventScroll: true })
                },
            )
        }

        // Follow the visual viewport so an on-screen keyboard shrinks the editor instead of
        // covering its bottom and the buttons.
        const viewport = window.visualViewport
        const update = () => {
            viewportTop = viewport.offsetTop
            viewportHeight = viewport.height
        }
        if (viewport) {
            update()
            viewport.addEventListener('resize', update)
            viewport.addEventListener('scroll', update)
        }
        return () => {
            mounted = false
            viewport?.removeEventListener('resize', update)
            viewport?.removeEventListener('scroll', update)
        }
    })
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
    class="fixed inset-x-0 z-modal flex bg-black/60 outline-hidden sm:items-center sm:justify-center sm:p-6 lg:p-10"
    style:top="{viewportTop}px"
    style:height={viewportHeight === null ? '100%' : `${viewportHeight}px`}
    tabindex="-1"
    role="presentation"
    data-text-editor-popup
    onkeydown={handleKeydown}
>
    <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="text-editor-popup-title"
        use:modalNavigation={{ close: cancel, leaveEscape }}
        class="flex h-full w-full flex-col bg-darkbg text-textcolor sm:max-w-6xl sm:rounded-xl sm:border sm:border-darkborderc sm:shadow-2xl"
    >
        <div class="flex shrink-0 items-center justify-between gap-3 border-b border-darkborderc pr-2 pl-4 pt-[max(0.5rem,env(safe-area-inset-top))] pb-2 sm:pt-2 sm:pl-5">
            <h2 id="text-editor-popup-title" class="min-w-0 truncate text-base font-semibold">{request.title ?? language.edit}</h2>
            <div class="flex shrink-0 items-center gap-1">
                {#if request.preview}
                    <button
                        type="button"
                        class="rounded-md px-3 py-1.5 text-sm text-textcolor2 transition-colors duration-200 hover:bg-selected hover:text-textcolor focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected"
                        onclick={togglePreview}
                    >{previewing ? language.edit : language.preview}</button>
                {/if}
                <button
                    type="button"
                    class="rounded-md p-2 text-textcolor2 transition-colors duration-200 hover:bg-selected hover:text-textcolor focus:outline-hidden focus-visible:ring-2 focus-visible:ring-selected"
                    aria-label={strings.close}
                    title={strings.close}
                    onclick={cancel}
                >
                    <XIcon size={20} />
                </button>
            </div>
        </div>
        <!-- The editor keeps its size under the preview, so it returns with the same layout and scroll. -->
        <div class="relative min-h-0 flex-1">
            {#if editorMode === 'plain'}
                <textarea
                    bind:this={textarea}
                    bind:value={draft}
                    oninput={(event) => request.input?.(event.currentTarget.value)}
                    class="absolute inset-0 h-full w-full resize-none bg-transparent px-4 py-3 text-base leading-relaxed text-textcolor outline-hidden sm:px-5 sm:py-4"
                    class:invisible={previewing}
                    aria-labelledby="text-editor-popup-title"
                    autocomplete="off"
                    spellcheck="false"
                ></textarea>
            {:else}
                <div bind:this={monacoHost} class="absolute inset-0 py-1" class:invisible={previewing}>
                    {#if Monaco}
                        <Monaco
                            bind:this={monaco}
                            bind:value={draft}
                            language={request.language ?? 'markdown'}
                            onchange={(value) => request.input?.(value)}
                            onsave={save}
                        />
                    {:else}
                        <div class="flex h-full items-center justify-center text-sm text-textcolor2">{language.loading}</div>
                    {/if}
                </div>
            {/if}
            {#if previewing && Preview}
                <div class="absolute inset-0 flex flex-col">
                    <Preview value={draft} presetEditor={request.presetEditor} />
                </div>
            {/if}
        </div>
        <div class="flex shrink-0 justify-end gap-2 border-t border-darkborderc px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] sm:px-5 sm:pb-3">
            <SettingButton variant="secondary" class="min-w-20" disabled={saving} onclick={cancel}>{language.cancel}</SettingButton>
            <SettingButton class="min-w-20" busy={saving} onclick={save}>{strings.save}</SettingButton>
        </div>
    </div>
</div>
