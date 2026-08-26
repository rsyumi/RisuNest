import { selectColdPayloadAuthority } from './coldPayloadAuthority'
import type { ColdPayloadAuthorityState, Versioned } from './persistentDataStore'
import type { ColdPayloadStore } from './coldPayloadStore'

interface ColdPayloadAuthorityReader {
    readColdPayloadAuthority(): Promise<Versioned<ColdPayloadAuthorityState>>
}

export async function selectRuntimeColdPayloadStore(input: {
    store: ColdPayloadAuthorityReader
    legacy: ColdPayloadStore
    v2?: ColdPayloadStore
    v2Capability: boolean
}): Promise<ColdPayloadStore> {
    const authority = await input.store.readColdPayloadAuthority()
    return selectColdPayloadAuthority(authority.value, {
        legacy: input.legacy,
        v2: input.v2,
        v2Capability: input.v2Capability,
    })
}

export function createRuntimeColdPayloadDispatcher(input: {
    store: ColdPayloadAuthorityReader
    legacy: ColdPayloadStore
    v2?: ColdPayloadStore
    v2Capability: boolean
}): ColdPayloadStore {
    const selected = () => selectRuntimeColdPayloadStore(input)
    return {
        async read(key) {
            return (await selected()).read(key)
        },
        async write(key, data) {
            const ownedData = data.slice()
            return (await selected()).write(key, ownedData)
        },
        async list() {
            return (await selected()).list()
        },
        async remove(key) {
            return (await selected()).remove(key)
        },
    }
}
