<script lang="ts">
    import { onMount, tick } from 'svelte'
    import Delete from '@lucide/svelte/icons/delete'
    import ScanFace from '@lucide/svelte/icons/scan-face'
    import FingerprintPattern from '@lucide/svelte/icons/fingerprint-pattern'
    import { language } from 'src/lang'
    import risuNestLogo from '../../../src-tauri/icon-src/logo.svg?url'
    import SettingButton from '../Setting/RisuNest/SettingButton.svelte'
    import SettingNotice from '../Setting/RisuNest/SettingNotice.svelte'

    interface Props {
        mode?: 'unlock' | 'setup' | 'change' | 'reset'
        phase?: 'entry' | 'confirm'
        count?: number
        compact?: boolean
        busy?: boolean
        nativeBusy?: boolean
        issue?: 'wrongPin' | 'mismatch' | 'nativeUnavailable' | null
        saveFailed?: boolean
        nativeAction?: 'unlock' | 'reset' | null
        nativeMethod?: 'windowsHello' | 'faceId' | 'touchId' | 'device'
        onDigit?: (digit: string) => void
        onDelete?: () => void
        onNative?: () => void
        onReset?: () => void
        onRetry?: () => void
        onCancel?: () => void
        onSurface?: (surface: { element: HTMLElement; keydown: (event: KeyboardEvent) => void } | null) => void
    }

    let { mode = 'unlock', phase = 'entry', count = 0, compact = false, busy = false,
        nativeBusy = false, issue = null, saveFailed = false, nativeAction = null,
        nativeMethod = 'device', onDigit, onDelete, onNative, onReset, onRetry, onCancel, onSurface }: Props = $props()
    let root: HTMLDivElement
    const text = $derived(language.risuNest.appLock)
    const title = $derived(mode === 'setup' ? text.setupTitle : mode === 'change' ? text.changeTitle : text.resetPin)
    const instruction = $derived(mode === 'unlock' ? text.enterPin : phase === 'confirm' ? text.confirmPin : text.newPin)
    const error = $derived(issue ? text[issue] : '')
    const blocked = $derived(busy || nativeBusy || saveFailed)
    const nativeLabel = $derived(nativeMethod === 'windowsHello' ? text.openWithWindowsHello
        : nativeMethod === 'faceId' ? text.openWithFaceId
        : nativeMethod === 'touchId' ? text.openWithTouchId : text.openWithDevice)

    function digit(value: string): void {
        if (!blocked) onDigit?.(value)
    }

    function removeDigit(): void {
        if (!blocked) onDelete?.()
    }

    export function handleKeydown(event: KeyboardEvent): void {
        if (event.isComposing) return
        if (event.ctrlKey || event.metaKey || event.altKey) {
            event.preventDefault()
            return
        }
        if (/^[0-9]$/.test(event.key)) {
            event.preventDefault()
            digit(event.key)
        } else if (event.key === 'Backspace') {
            event.preventDefault()
            removeDigit()
        } else if (event.key === 'Tab') {
            event.preventDefault()
            const buttons = [...root.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')]
            const current = buttons.indexOf(document.activeElement as HTMLButtonElement)
            const next = current < 0 ? (event.shiftKey ? buttons.length - 1 : 0)
                : (current + (event.shiftKey ? -1 : 1) + buttons.length) % buttons.length
            ;(buttons[next] ?? root).focus()
        } else if (event.key === 'Enter' || event.key === ' ') {
            event.preventDefault()
            const target = document.activeElement
            if (target instanceof HTMLButtonElement && root.contains(target) && !target.disabled) target.click()
        } else if (event.key === 'Escape') {
            event.preventDefault()
        }
    }

    $effect(() => {
        const currentMode = mode
        void tick().then(() => {
            if (mode === currentMode && root?.isConnected) root.focus({ preventScroll: true })
        })
    })

    onMount(() => {
        root.focus({ preventScroll: true })
        onSurface?.({ element: root, keydown: handleKeydown })
        return () => onSurface?.(null)
    })
</script>

<div bind:this={root} class="app-lock-screen fixed inset-0 z-app-lock bg-bgcolor text-textcolor" class:compact
    role="dialog" aria-modal="true" aria-label={mode === 'unlock' ? 'RisuNest' : title}
    tabindex="-1" onkeydown={handleKeydown}>
    {#if mode !== 'unlock'}
        <div class="cancel"><SettingButton variant="quiet" disabled={busy || nativeBusy} onclick={onCancel}>{language.cancel}</SettingButton></div>
    {/if}
    <div class="column">
        <div class="info">
            {#if mode === 'unlock'}
                <img class="logo" src={risuNestLogo} alt="" />
                <div class="name">RisuNest</div>
            {:else}
                <h2 class="title">{title}</h2>
            {/if}
            <div class="instruction" aria-live="polite">{instruction}</div>
            <div class="dots" class:shake={issue === 'mismatch' || (issue === 'wrongPin' && count === 6)}
                role="img" aria-label={text.progress.replace('{0}', String(count))}>
                {#each [0, 1, 2, 3, 4, 5] as index}
                    <span class="dot" class:filled={index < count}></span>
                {/each}
            </div>
            <div class="error" role="alert">{error}</div>
            {#if saveFailed}
                <div class="save-failure">
                    <SettingNotice tone="danger" text={language.risuNest.localData.applyFailed} role="alert" />
                    <SettingButton busy={busy} onclick={onRetry}>{language.retry}</SettingButton>
                </div>
            {/if}
        </div>
        <div class="keypad" class:disabled={saveFailed}>
            {#each ['1', '2', '3', '4', '5', '6', '7', '8', '9', '', '0', 'delete'] as key}
                {#if key === ''}<span aria-hidden="true"></span>
                {:else if key === 'delete'}
                    <button type="button" class="key delete" disabled={blocked} aria-label={text.deleteDigit} onclick={removeDigit}>
                        <Delete size={26} strokeWidth={1.75} aria-hidden="true" />
                    </button>
                {:else}
                    <button type="button" class="key" disabled={blocked} onclick={() => digit(key)}>{key}</button>
                {/if}
            {/each}
        </div>
        <div class="footer">
            {#if mode === 'unlock' && nativeAction === 'unlock'}
                <button type="button" class="native" disabled={busy || nativeBusy} onclick={onNative}>
                    {#if nativeMethod === 'windowsHello' || nativeMethod === 'faceId'}
                        <ScanFace size={18} aria-hidden="true" />
                    {:else}<FingerprintPattern size={18} aria-hidden="true" />{/if}
                    {nativeLabel}
                </button>
            {:else if mode === 'unlock' && nativeAction === 'reset'}
                <SettingButton variant="quiet" disabled={busy || nativeBusy} onclick={onReset}>{text.resetPin}</SettingButton>
            {/if}
        </div>
    </div>
</div>

<style>
    .app-lock-screen { display: flex; overflow-y: auto; padding: calc(24px + env(safe-area-inset-top, 0px)) calc(16px + env(safe-area-inset-right, 0px)) calc(24px + env(safe-area-inset-bottom, 0px)) calc(16px + env(safe-area-inset-left, 0px)); line-height: 1.5; outline: none; }
    .cancel { position: absolute; top: calc(10px + env(safe-area-inset-top, 0px)); left: calc(10px + env(safe-area-inset-left, 0px)); z-index: 1; }
    .column { display: flex; flex-direction: column; align-items: center; width: 100%; max-width: 320px; margin: auto; text-align: center; }
    .info { display: flex; flex-direction: column; align-items: center; width: 100%; max-width: 320px; }
    .logo { display: block; width: 56px; height: 56px; border-radius: 14px; }
    .compact .logo { width: 48px; height: 48px; border-radius: 12px; }
    .name { margin-top: 8px; font-size: 17px; font-weight: 700; }
    .title { margin: 0; font-size: 20px; font-weight: 700; line-height: 1.4; }
    .instruction { margin-top: 24px; font-size: 15px; }
    .compact .instruction { margin-top: 20px; }
    .dots { display: flex; gap: 14px; margin-top: 14px; }
    .dot { width: 12px; height: 12px; border-radius: 50%; border-width: 1.5px; border-style: solid; border-color: color-mix(in srgb, var(--risu-theme-textcolor) 45%, transparent); transition: background-color .12s, border-color .12s; }
    .dot.filled { background: var(--risu-theme-textcolor); border-color: var(--risu-theme-textcolor); }
    .dots.shake { animation: shake .36s; }
    @keyframes shake { 0%, 100% { transform: translateX(0); } 20% { transform: translateX(-8px); } 40% { transform: translateX(7px); } 60% { transform: translateX(-5px); } 80% { transform: translateX(3px); } }
    @media (prefers-reduced-motion: reduce) { .dots.shake { animation: none; } }
    .error { min-height: 20px; margin-top: 10px; font-size: 13px; line-height: 20px; color: var(--risu-theme-danger-400); }
    .save-failure { width: 100%; display: grid; gap: 8px; justify-items: center; margin: 2px 0 4px; text-align: left; }
    .keypad { display: grid; grid-template-columns: repeat(3, 72px); gap: 14px 24px; margin-top: 14px; }
    .compact .keypad { grid-template-columns: repeat(3, 60px); gap: 10px 22px; margin-top: 10px; }
    .key { display: flex; align-items: center; justify-content: center; width: 72px; height: 72px; border: 0; border-radius: 50%; background: color-mix(in srgb, var(--risu-theme-darkbutton) 72%, transparent); color: var(--risu-theme-textcolor); font-size: 26px; font-weight: 500; font-variant-numeric: tabular-nums; transition: background-color .1s; }
    .compact .key { width: 60px; height: 60px; font-size: 22px; }
    .key:enabled:hover { background: var(--risu-theme-selected); }
    .key:enabled:active { background: color-mix(in srgb, var(--risu-theme-selected) 75%, var(--risu-theme-textcolor) 25%); }
    .key.delete { background: transparent; color: var(--color-subtext); }
    .key.delete:enabled:hover { background: color-mix(in srgb, var(--risu-theme-selected) 60%, transparent); }
    .key:focus-visible, .native:focus-visible { outline: 2px solid var(--risu-theme-borderc); outline-offset: 2px; }
    .keypad.disabled { opacity: .4; }
    .footer { display: flex; align-items: center; justify-content: center; min-height: 42px; margin-top: 18px; }
    .compact .footer { margin-top: 14px; }
    .native { display: inline-flex; align-items: center; gap: 8px; border: 1px solid var(--risu-theme-darkborderc); border-radius: 999px; padding: 8px 16px; background: transparent; color: var(--risu-theme-textcolor); font-size: 14px; }
    .native:enabled:hover { background: var(--risu-theme-selected); }
    .native:disabled { opacity: .5; cursor: default; }
    @media (max-height: 599px) and (min-width: 560px) {
        .column { display: grid; grid-template-columns: auto auto; grid-template-areas: 'info pad' 'foot pad'; column-gap: 48px; align-items: center; justify-items: center; width: auto; max-width: none; }
        .info { grid-area: info; align-self: end; }
        .app-lock-screen .keypad { grid-area: pad; margin-top: 0; grid-template-columns: repeat(3, 60px); gap: 10px 22px; }
        .app-lock-screen .key { width: 60px; height: 60px; font-size: 22px; }
        .app-lock-screen .footer { grid-area: foot; align-self: start; margin-top: 8px; }
        .app-lock-screen .logo { width: 48px; height: 48px; border-radius: 12px; }
        .app-lock-screen .instruction { margin-top: 20px; }
    }
</style>