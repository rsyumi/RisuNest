<script lang="ts">
    // settled() resolves once the DOM update commits; tick() would also wait for a frame or timer.
    import { onDestroy, settled as domSettled, untrack } from 'svelte'
    import { ParseMarkdown } from 'src/ts/parser/parser.svelte'
    import { language } from 'src/lang'
    import {
        DeferredInlayMarkerRegistry,
        mountDeferredInlaySources,
    } from 'src/ts/process/files/inlayRenderSource'

    type MarkdownCharacter = Parameters<typeof ParseMarkdown>[1]
    type MarkdownMode = Parameters<typeof ParseMarkdown>[2]
    type MarkdownConditions = Parameters<typeof ParseMarkdown>[4]

    interface Props {
        data: string
        /** The parent element holding only this markdown, which renders as its direct children. */
        root: HTMLElement | null | undefined
        character?: MarkdownCharacter
        mode?: MarkdownMode
        chatID?: number
        conditions?: MarkdownConditions
        signal?: AbortSignal
        preservePendingContent?: boolean
        scriptContext?: Parameters<typeof ParseMarkdown>[5]['scriptContext']
        /** A parse started after this changes rebuilds identical markup instead of keeping its DOM. */
        rebuildRevision?: number
    }

    let {
        data,
        root,
        character = null,
        mode = 'normal',
        chatID = -1,
        conditions = {},
        signal,
        preservePendingContent = false,
        scriptContext,
        rebuildRevision = 0,
    }: Props = $props()
    let displayedHtml = $state('')
    let displayEpoch = $state(0)
    let failed = $state(false)
    let activeJob: ReturnType<typeof startParsing> | null = null
    let displayedJob: ReturnType<typeof startParsing> | null = null
    let destroyed = false

    function startParsing() {
        const registry = new DeferredInlayMarkerRegistry()
        const controller = new AbortController()
        const externalSignal = signal
        const abort = () => controller.abort(externalSignal?.reason)
        if (externalSignal?.aborted) abort()
        else externalSignal?.addEventListener('abort', abort, { once: true })
        return {
            registry,
            controller,
            detach: () => externalSignal?.removeEventListener('abort', abort),
            release: () => {},
            disposed: false,
            settled: false,
            rebuildRevision: untrack(() => rebuildRevision),
            promise: ParseMarkdown(data, character, mode, chatID, conditions, {
                deferredInlays: registry,
                signal: controller.signal,
                scriptContext,
            }),
        }
    }

    const parseJob = $derived.by(startParsing)

    function dispose(job: ReturnType<typeof startParsing> | null) {
        if (!job || job.disposed) return
        job.disposed = true
        job.controller.abort()
        job.detach()
        job.release()
        job.registry.clear()
    }

    async function mountSources(job: ReturnType<typeof startParsing>) {
        const html = await job.promise
        if (
            destroyed ||
            job.disposed ||
            job.controller.signal.aborted ||
            job !== parseJob
        ) {
            if (job !== displayedJob) dispose(job)
            return
        }
        const previous = displayedJob
        const retainMarkup =
            html === displayedHtml && previous?.settled && job.rebuildRevision === previous.rebuildRevision
        if (retainMarkup) {
            job.release = previous.release
            previous.release = () => {}
            job.registry.clear()
        } else if (html === displayedHtml) displayEpoch += 1
        displayedJob = job
        displayedHtml = html
        await domSettled()
        dispose(previous)
        if (destroyed || job.disposed || job !== parseJob) {
            if (job !== displayedJob) dispose(job)
            return
        }
        if (retainMarkup) job.settled = true
        else if (root) {
            job.release = mountDeferredInlaySources(root, job.registry)
            job.settled = true
        } else job.registry.clear()
    }

    $effect(() => {
        const job = parseJob
        if (activeJob !== job) {
            activeJob?.controller.abort()
            activeJob?.detach()
            if (activeJob !== displayedJob) dispose(activeJob)
            activeJob = job
        }
        if (!preservePendingContent) {
            displayedHtml = ''
            dispose(displayedJob)
            displayedJob = null
        }
        failed = false
        // A failed parse still has to release the job's registry; letting the
        // rejection escape leaks it and reports an unhandled rejection.
        void mountSources(job).catch((error) => {
            if (job !== displayedJob) job.registry.clear()
            if (destroyed || job.controller.signal.aborted || job !== activeJob)
                return
            failed = true
            console.error('Deferred markdown render failed', error)
        })
    })

    onDestroy(() => {
        destroyed = true
        dispose(activeJob)
        dispose(displayedJob)
    })

</script>

{#key displayEpoch}
    {@html displayedHtml}
{/key}
{#if failed}
    <span class="text-textcolor2 italic" role="alert">{language.contentRenderFailed}</span>
{/if}
