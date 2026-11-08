<script lang="ts">
    import { CheckIcon, CopyIcon, XIcon } from '@lucide/svelte'
    import { language } from 'src/lang'
    import { diagnosticInfo, formatErrorDetails } from 'src/ts/diagnostics'

    let { message }: { message: string } = $props()
    let open = $state(false)
    let copied = $state('')
    let failed = $state('')
    const report = $derived(formatErrorDetails($diagnosticInfo, message))

    async function copy() {
        const text = report
        let done = false
        try {
            await navigator.clipboard.writeText(text)
            done = true
        } catch {
            const textarea = document.createElement('textarea')
            textarea.value = text
            document.body.appendChild(textarea)
            textarea.select()
            try { done = document.execCommand('copy') }
            catch { /* Keep the report selectable when the clipboard is unavailable. */ }
            finally { textarea.remove() }
        }
        copied = done ? text : ''
        failed = done ? '' : text
    }
</script>

<details bind:open class="mt-3 min-w-0 w-full text-sm">
    <summary class="cursor-pointer text-subtext">{open ? language.hideErrorDetails : language.showErrorDetails}</summary>
    <div class="relative mt-2">
        <button type="button" class="absolute top-1 right-1 rounded-sm bg-bgcolor p-1 text-subtext"
            title={copied === report ? language.copied : failed === report ? language.risuNest.networkLog.clipboardDenied : language.copy}
            aria-label={language.copy} onclick={copy}>
            {#if copied === report}<CheckIcon size={14} />{:else if failed === report}<XIcon size={14} />{:else}<CopyIcon size={14} />{/if}
        </button>
        <pre class="max-h-48 overflow-auto rounded-md border border-darkborderc bg-bgcolor p-2 pr-8 text-xs whitespace-pre-wrap break-all text-textcolor">{report}</pre>
    </div>
</details>
