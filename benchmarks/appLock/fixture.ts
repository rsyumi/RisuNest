import { invoke } from '@tauri-apps/api/core'
import { createAppLockNativeAdapter, type AppLockLifecycle } from '../../src/ts/appLock/native'
import { APP_LOCK_KEY, defaultAppLockRecord } from '../../src/ts/appLock/record'

const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))
async function until(condition: () => boolean, message: string, timeout = 90_000) {
    const deadline = performance.now() + timeout
    while (!condition()) {
        if (performance.now() > deadline) throw new Error(message)
        await pause(25)
    }
}
const frame = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))

export async function appLockFixture(platform: 'ios' | 'macos' | 'linux', phase: string, initialize: () => Promise<unknown>,
    sendReport?: (stage: string, result: unknown) => Promise<unknown>) {
    if (platform === 'linux' && !sendReport) throw new Error('Linux fixture requires an external report reader')
    const report = sendReport ?? ((stage: string, result: unknown) => invoke(`${platform}_bench_report`, { stage, result }))
    const timed = phase === 'app-lock-timed' || phase === 'app-lock-timed-mutant'
    const ui = phase === 'app-lock-ui'
    const mutant = phase.endsWith('-mutant')
    Object.assign(window, { __risunestAppLockRemoveCover: mutant })
    let armed = false
    let departure: AppLockLifecycle | null = null
    let returned: AppLockLifecycle | null = null
    let blockedMs = 0
    let busyStartedAtMs: number | null = null
    // Install before the product subscribes so stale content remains painted while JS is busy.
    await createAppLockNativeAdapter().listen(snapshot => {
        if (!armed) return
        if (!snapshot.active && !departure) departure = snapshot
        if (snapshot.active && departure && snapshot.epoch > departure.epoch) returned = snapshot
        if (ui || blockedMs !== 0 || (!timed && snapshot.active) || (timed && !snapshot.active)) return
        busyStartedAtMs = snapshot.atMs
        const started = Date.now()
        const duration = timed ? 2_000 : 8_000
        while (Date.now() - started < duration) { /* Deliberately occupy only the synthetic WebView. */ }
        blockedMs = Date.now() - started
    })
    await initialize()
    const { createNativeDeviceSettings } = await import('../../src/ts/storage/nativeDeviceSettings')
    const settings = createNativeDeviceSettings()
    await settings.set(APP_LOCK_KEY, JSON.stringify(defaultAppLockRecord()))
    await settings.set('risunest_tos_v1', 'true')
    const opened = await invoke<{ revision: number }>('pds_open')
    await invoke('pds_commit', { commit: { expectedRevision: opened.revision,
        rootMutations: [{ type: 'set', key: 'didFirstSetup', value: true }] }, assetAliases: [] })
    document.getElementById('benchmark')?.remove()
    const product = await import('../../src/main')
    await product.default
    const { appLock } = await import('../../src/ts/appLock/appLock.svelte')
    const { DBState } = await import('../../src/ts/stores.svelte')
    const { changeChar } = await import('../../src/ts/characters')
    await until(() => appLock.state.initialized && appLock.state.active && !appLock.gated
        && performance.getEntriesByName('boot:interactive').length > 0, 'Product content did not become ready')
    const index = DBState.db.characters.findIndex(character => character.chaId === 'char-a')
    if (index < 0 || !await changeChar(index)) throw new Error('Synthetic character did not open')
    appLock.beginSetup()
    for (const digit of '123456123456') await appLock.digit(digit)
    if (!appLock.state.record.enabled || appLock.gated) throw new Error('Public PIN setup did not complete')
    if (!await appLock.setTiming(timed ? 'after-1m' : 'immediate')) throw new Error('Lock timing did not save')
    if (platform === 'ios' && !await appLock.setHideRecentsPreview(!ui)) throw new Error('Preview policy did not save')
    const main = document.querySelector('main')!
    const retained = document.createElement('section')
    retained.style.cssText = 'position:fixed;left:24px;top:96px;z-index:500;display:grid;gap:12px;background:#202020;padding:8px'
    const marker = document.createElement('div')
    marker.style.cssText = 'width:128px;height:96px;background:linear-gradient(90deg,#ff00ff 25%,#00ffff 25%,#00ffff 50%,#ff00ff 50%,#ff00ff 75%,#00ffff 75%)'
    const draft = document.createElement('textarea')
    draft.setAttribute('aria-label', 'Retained synthetic draft')
    draft.value = 'retained synthetic draft 123456'
    const status = document.createElement('span')
    status.textContent = 'app-lock-ready'
    retained.append(marker, draft, status)
    main.append(retained)
    await frame()
    await appLock.rendered('Synthetic app lock')
    armed = true
    await report('app-lock-ready', { timed, mutant, pin: '123456', viewport: { width: innerWidth, height: innerHeight },
        marker: marker.getBoundingClientRect().toJSON(), epoch: appLock.state.epoch, hideRecentsPreview: appLock.state.record.hideRecentsPreview })
    await until(() => returned !== null && appLock.state.locked && appLock.state.flow === 'unlock'
        && !appLock.state.returnPending && !appLock.state.busy, 'Current return gate did not settle', timed ? 120_000 : 60_000)
    const away = departure as AppLockLifecycle | null
    const active = returned as AppLockLifecycle | null
    if (!away || !active || active.epoch <= away.epoch || active.departedAtMs !== away.departedAtMs) throw new Error('Native lifecycle trace mismatch')
    if (timed && active.atMs - away.atMs < 60_000) throw new Error('Timed return did not cross the real one-minute threshold')
    const blockedAfterReturnMs = timed ? blockedMs : blockedMs - (active.atMs - away.atMs)
    if (!ui && blockedAfterReturnMs < 750) throw new Error('Return did not overlap the busy WebView interval')
    if (document.querySelector('main') !== main || !main.contains(retained) || draft.value !== 'retained synthetic draft 123456' || !main.inert) {
        throw new Error('Locked content was replaced or remained interactive')
    }
    await frame()
    const keys = [...document.querySelectorAll<HTMLButtonElement>('.app-lock-screen button.key')]
        .filter(button => /^[1-9]$/.test(button.textContent?.trim() ?? '')).map(button => button.getBoundingClientRect().toJSON())
    if (keys.length !== 9) throw new Error('Actual shared PIN keypad is missing')
    await appLock.rendered('Synthetic app lock')
    await report('app-lock-locked', { timed, mutant, departure: away, returned: active, blockedMs, busyStartedAtMs,
        blockedAfterReturnMs, retained: true, keys })
    if (ui) {
        await until(() => !appLock.gated, 'PIN keypad did not unlock the retained page', 60_000)
        if (document.querySelector('main') !== main || !main.contains(draft) || draft.value !== 'retained synthetic draft 123456') {
            throw new Error('PIN unlock did not retain the same draft element')
        }
        if (!await appLock.setEnabled(false)) throw new Error('Synthetic lock cleanup failed')
        status.textContent = 'app-lock-retained'
        await report('complete', { passed: true, retained: true })
    }
}
