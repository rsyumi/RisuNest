<script module lang="ts">
    import { getContext, setContext } from 'svelte'

    const key = Symbol('chat-window-layer')

    /** Gives dialogs in the chat screen that cover the whole window a layer outside the chat pane's stacking context and paint containment. */
    export function provideChatWindowLayer(layer: () => HTMLElement | undefined) {
        setContext(key, layer)
    }

    /** Chat rows mounted on their own inherit no context, so they take the layer through this. */
    export function chatWindowLayerContext() {
        const layer = getContext(key)
        return new Map<symbol, unknown>(layer ? [[key, layer]] : [])
    }
</script>

<script lang="ts">
    import { getAllContexts, mount, unmount, type Snippet } from 'svelte'
    import PortalConsumer from 'src/lib/UI/GUI/PortalConsumer.svelte'

    let { children }: { children: Snippet } = $props()
    // Outside a chat screen, as in the bookmark list, the dialog stays where it is rendered.
    const layer = getContext<(() => HTMLElement | undefined) | undefined>(key)
    const context = getAllContexts()

    // The chat screen binds its layer after it renders, so a dialog rendered with it waits for the layer.
    $effect(() => {
        const target = layer?.()
        if (!target) return
        const instance = mount(PortalConsumer, { target, props: { children }, context })
        return () => {
            void unmount(instance)
        }
    })
</script>

{#if !layer}
    {@render children()}
{/if}
