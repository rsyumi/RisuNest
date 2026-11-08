<script lang="ts">
    import { onMount } from 'svelte'
    import QRCode from 'qrcode'
    import SettingButton from '../RisuNest/SettingButton.svelte'
    import { externalErrorKind, externalErrorMessage, type ExternalStorageStrings } from './strings'

    let { payload, strings, onsave, onclose }: {
        payload: string
        strings: ExternalStorageStrings
        onsave: () => Promise<void>
        onclose: () => void
    } = $props()

    let backdrop: HTMLDivElement
    let panel: HTMLDivElement
    let heading: HTMLDivElement
    let controls: HTMLDivElement
    let footer: HTMLDivElement
    let text: HTMLTextAreaElement
    let qr = $state('')
    let qrPending = $state(true)
    let modules = $state(0)
    let targetSize = $state(0)
    let size = $state(0)
    let copying = $state(false)
    let saving = $state(false)
    let feedback = $state('')
    let error = $state('')

    function fitQr(): void {
        if (!modules || !backdrop || !panel) return
        const outer = getComputedStyle(backdrop)
        const inner = getComputedStyle(panel)
        const width = panel.clientWidth - parseFloat(inner.paddingLeft) - parseFloat(inner.paddingRight)
        const height = backdrop.clientHeight - parseFloat(outer.paddingTop) - parseFloat(outer.paddingBottom)
            - parseFloat(inner.paddingTop) - parseFloat(inner.paddingBottom) - 2
            - heading.offsetHeight - controls.offsetHeight - footer.offsetHeight - parseFloat(inner.rowGap) * 3
        // Whole CSS pixels per module preserve sharp edges, including the quiet zone.
        // Short viewports scroll at the original size instead of making the QR unreadable.
        const available = Math.max(Math.min(targetSize / 2, width), Math.min(targetSize, width, height))
        size = Math.max(1, Math.floor(available / modules)) * modules
    }

    async function copy(): Promise<void> {
        copying = true
        feedback = ''
        error = ''
        try {
            await navigator.clipboard.writeText(payload)
            feedback = strings.connectionSettingsCopied
        } catch {
            error = strings.connectionSettingsCopyFailed
            text.focus()
            text.select()
        } finally {
            copying = false
        }
    }

    async function save(): Promise<void> {
        saving = true
        feedback = ''
        error = ''
        try {
            await onsave()
        } catch (reason) {
            if (externalErrorKind(reason) !== 'cancelled') error = externalErrorMessage(strings, reason)
        } finally {
            saving = false
        }
    }

    function keydown(event: KeyboardEvent): void {
        if (event.key === 'Escape') { event.stopPropagation(); onclose() }
        if (event.key !== 'Tab') return
        const focusable = [...panel.querySelectorAll<HTMLElement>('textarea, button:not(:disabled)')]
        const first = focusable[0]
        const last = focusable.at(-1)
        if (event.shiftKey && (document.activeElement === first || document.activeElement === panel)) {
            event.preventDefault()
            last?.focus()
        } else if (!event.shiftKey && document.activeElement === last) {
            event.preventDefault()
            first?.focus()
        }
    }

    onMount(() => {
        const previous = document.activeElement
        panel.focus()
        let disposed = false
        let observer: ResizeObserver | undefined
        void (async () => {
            try {
                if (!payload || payload.length > 2_300) return
                const count = QRCode.create(payload).modules.size + 8
                const scale = Math.ceil(240 / count) * 2
                const image = await QRCode.toDataURL(payload, { margin: 4, scale })
                if (disposed) return
                modules = count
                targetSize = count * scale
                size = count * scale / 2
                qr = image
            } catch {
                qr = ''
            } finally {
                if (!disposed) qrPending = false
            }
        })()
        observer = new ResizeObserver(fitQr)
        for (const element of [backdrop, panel, heading, controls, footer]) observer.observe(element)
        return () => {
            disposed = true
            observer?.disconnect()
            if (previous instanceof HTMLElement && previous.isConnected) previous.focus()
        }
    })
</script>

<div bind:this={backdrop} class="backdrop">
    <div bind:this={panel} tabindex="-1" role="dialog" aria-modal="true" aria-labelledby="external-connection-settings-title"
        class="panel outline-hidden" style:--qr-target={`${targetSize}px`} onkeydown={keydown}>
        <div bind:this={heading} class="heading">
            <h3 id="external-connection-settings-title" class="text-lg font-bold">{strings.connectionSettings}</h3>
            <p class="text-sm text-subtext">{strings.connectionSettingsNotice}</p>
        </div>
        {#if qr}
            <img class="qr" src={qr} width={size} height={size} alt={strings.connectionSettingsQr} />
        {:else if qrPending}
            <p class="text-sm text-subtext" role="status">{strings.loading}</p>
        {:else}
            <p class="rounded-md border border-darkborderc bg-darkbg p-3 text-sm">{strings.connectionSettingsFileOnly}</p>
        {/if}
        <div bind:this={controls} class="controls">
            <textarea bind:this={text} aria-label={strings.connectionSettingsPayload} readonly value={payload}
                rows="3" spellcheck="false" autocapitalize="off"
                class="w-full rounded-md border border-darkborderc bg-darkbg p-2 font-mono text-xs text-textcolor shadow-xs"></textarea>
            <div class="feedback">
                <span class="text-sm text-subtext" role="status">{feedback}</span>
                {#if error}<p class="text-sm text-draculared" role="alert">{error}</p>{/if}
            </div>
        </div>
        <div bind:this={footer} class="actions">
            <SettingButton variant="secondary" busy={copying} onclick={copy}>{strings.copyConnectionSettings}</SettingButton>
            <SettingButton busy={saving} onclick={save}>{strings.saveConnectionSettings}</SettingButton>
            <SettingButton variant="secondary" onclick={onclose}>{strings.closeRecovery}</SettingButton>
        </div>
    </div>
</div>

<style>
    .backdrop {
        position: fixed;
        inset: 0;
        z-index: 1000;
        display: flex;
        align-items: center;
        justify-content: center;
        padding: max(1rem, env(safe-area-inset-top)) max(1rem, env(safe-area-inset-right)) max(1rem, env(safe-area-inset-bottom)) max(1rem, env(safe-area-inset-left));
        background: rgb(0 0 0 / 60%);
    }
    .panel {
        display: flex;
        flex-direction: column;
        gap: 1rem;
        width: min(100%, max(28rem, calc(var(--qr-target) + 2.5rem + 2px)));
        max-height: 100%;
        overflow-y: auto;
        padding: 1.25rem;
        border: 1px solid var(--risu-theme-darkborderc);
        border-radius: 0.5rem;
        background: var(--risu-theme-bgcolor);
        color: var(--risu-theme-textcolor);
    }
    .panel > * { flex-shrink: 0; }
    .heading { display: grid; gap: 0.5rem; }
    .qr { align-self: center; max-width: 100%; height: auto; image-rendering: pixelated; }
    .controls { display: grid; gap: 0.5rem; min-width: 0; }
    textarea { resize: none; overflow-wrap: anywhere; }
    .feedback { min-height: 1.25rem; line-height: 1.25rem; }
    .actions { display: flex; flex-wrap: wrap; justify-content: flex-end; gap: 0.5rem; }
</style>
