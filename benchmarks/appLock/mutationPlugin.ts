import { fileURLToPath } from 'node:url'
import type { Plugin } from 'vite'

export function appLockMutationPlugin(): Plugin {
    const nativePath = fileURLToPath(new URL('../../src/ts/appLock/native.ts', import.meta.url)).replaceAll('\\', '/')
    const seam = "const snapshot = await invoke<AppLockLifecycle>('app_lock_set_policy', { policy })"
    return {
        name: 'synthetic-app-lock-cover-mutation',
        enforce: 'pre',
        transform(code, id) {
            if (id.split('?')[0].replaceAll('\\', '/') !== nativePath) return
            if (code.split(seam).length !== 2) throw new Error('App-lock policy mutation seam changed')
            return code.replace(seam, `const snapshot = await invoke<AppLockLifecycle>('app_lock_set_policy', {
                policy: (globalThis as typeof globalThis & { __risunestAppLockRemoveCover?: boolean }).__risunestAppLockRemoveCover
                    ? { ...policy, enabled: false, hideRecentsPreview: false } : policy,
            })`)
        },
    }
}
