import { afterEach, expect, it, vi } from 'vitest'
import { tick } from 'svelte'
import { createNativeLocalUrlInitializer } from './nativeLocalUrls'
import { serverSyncNavigation } from './storage/sync/serverSyncDeepLink'
import { serverRegistrationInbox } from './storage/sync/serverSyncRegistrationInbox'
import vector from '../../crates/sync-connect/tests/registration-vector.json'

afterEach(() => serverRegistrationInbox.clear())
it('installs once and handles cold and warm server intents before onboarding has finished', async () => {
    let listener!: (urls: string[]) => void
    const onOpenUrl = vi.fn(async (receive: (urls: string[]) => void) => {
        listener = receive
        return () => {}
    })
    const getCurrent = vi.fn(async () => [vector.uri])
    const start = createNativeLocalUrlInitializer({ onOpenUrl, getCurrent })
    const navigation = vi.fn()
    const unsubscribe = serverSyncNavigation.subscribe(navigation)
    const onRealm = vi.fn()
    await Promise.all([start(onRealm), start(onRealm)])
    await tick()
    expect(onOpenUrl).toHaveBeenCalledOnce()
    expect(getCurrent).toHaveBeenCalledOnce()
    expect(navigation).toHaveBeenCalledExactlyOnceWith({})
    expect(serverRegistrationInbox.take()).toEqual(vector.registration)
    listener([vector.uri])
    await tick()
    expect(navigation).toHaveBeenCalledOnce()
    expect(serverRegistrationInbox.take()).toBeUndefined()
    listener(['risunestlocal://sync-server/connect?libraryId=another'])
    expect(navigation).toHaveBeenLastCalledWith({ libraryId: 'another' })
    expect(onRealm).not.toHaveBeenCalled()
    unsubscribe()
})
