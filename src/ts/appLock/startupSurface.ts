import { createAppLockNativeAdapter, type AppLockLifecycle } from './native'

const RETRY_DELAYS_MS = [32, 128, 512]
const DEFAULT_BACKGROUND = '#21252b'

export interface StartupSurface {
    /** Presents the current surface and settles when this attempt ends. Never rejects. */
    present(backgroundColor?: string): Promise<void>
    /** Stops presenting the surface again after later returns. */
    stop(): void
}

/**
 * Releases the native launch cover for a startup surface that shows no library content,
 * such as a startup panel or a boot failure, before the app lock record can be read.
 * The surface keeps the lock policy off and is presented again after each return.
 */
export function createStartupSurface(): StartupSurface {
    const native = createAppLockNativeAdapter()
    let stopped = false
    let unlisten: (() => void) | null = null
    let presenting: Promise<void> | null = null
    let presentation = 0
    let latestEpoch = -1
    const show = async (snapshot: AppLockLifecycle) => {
        if (snapshot.epoch < latestEpoch) return
        latestEpoch = snapshot.epoch
        const request = ++presentation
        if (!snapshot.active) return
        for (let attempt = 0; attempt <= RETRY_DELAYS_MS.length; attempt++) {
            if (stopped || request !== presentation) return
            try { if (await native.ready(snapshot.epoch, false)) return } catch { return }
            if (attempt < RETRY_DELAYS_MS.length) await new Promise<void>((resolve) => setTimeout(resolve, RETRY_DELAYS_MS[attempt]))
        }
    }
    const attempt = async (backgroundColor: string) => {
        if (!unlisten) {
            try {
                const off = await native.listen((snapshot) => { void show(snapshot) })
                if (stopped) off()
                else unlisten = off
            } catch { /* The policy snapshot below can still release the cover. */ }
        }
        if (stopped) return
        const snapshot = await native.setPolicy({
            enabled: false, timing: 'launch', gateRequired: false,
            hideRecentsPreview: false, backgroundColor,
        })
        await show(snapshot)
    }
    return {
        present(backgroundColor = DEFAULT_BACKGROUND) {
            presenting ??= attempt(backgroundColor).catch(() => undefined).finally(() => { presenting = null })
            return presenting
        },
        stop() {
            stopped = true
            unlisten?.()
            unlisten = null
        },
    }
}
