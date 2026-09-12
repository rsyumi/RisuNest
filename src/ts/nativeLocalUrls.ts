import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link'
import { dispatchRisuLocalUrl } from './deepLinkDispatcher'
import { serverSyncNavigation } from './storage/sync/serverSyncDeepLink'
import { receiveServerRegistration } from './storage/sync/serverSyncRegistrationDispatch'

/** Installed once during local startup, including first-run onboarding. */
export function createNativeLocalUrlInitializer(
    api = { getCurrent, onOpenUrl },
) {
    let started: Promise<void> | undefined
    return (onRealm: (id: string) => void): Promise<void> => {
        started ??= (async () => {
            const receive = (urls: string[]) => {
                for (const uri of urls)
                    dispatchRisuLocalUrl(uri, {
                        onRealm,
                        onServerSync: (value) => {
                            serverSyncNavigation.receive(value)
                        },
                        onServerRegistration: (value) => {
                            void receiveServerRegistration(value, () => {
                                serverSyncNavigation.receive(
                                    'risunestlocal://sync-server/connect',
                                )
                            })
                        },
                    })
            }
            // Subscribe before reading the launch intent, closing the startup delivery gap.
            await api.onOpenUrl(receive)
            receive((await api.getCurrent()) ?? [])
        })().catch(() => {
            // Native failures can contain the launch URI. Never log the raw error.
            console.warn('native-uri-initialization-failed')
        })
        return started
    }
}
export const initializeNativeLocalUrls = createNativeLocalUrlInitializer()
