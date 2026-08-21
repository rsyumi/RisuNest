import type { ActiveColdRootResolver } from './coldPayloadStore'
import type { ActiveBlobRootResolver } from './platformBlobStore'
import type { ActivePersistentTuple, PersistentDataStore } from './persistentDataStore'
import { assertGeneratedStorageRootId, type BlobStorageRoot } from './storageRoot'

function rootFromTuple(tuple: ActivePersistentTuple): BlobStorageRoot {
    if (tuple.payloadGeneration === 'legacy') return { kind: 'legacy' }
    assertGeneratedStorageRootId(tuple.payloadGeneration)
    return { kind: 'generation', id: tuple.payloadGeneration }
}

export class ActivePayloadRoot implements ActiveBlobRootResolver, ActiveColdRootResolver {
    private tuple: ActivePersistentTuple | undefined

    constructor(private readonly store: Pick<PersistentDataStore, 'readActiveTuple'>) {}

    install(tuple: ActivePersistentTuple): void {
        rootFromTuple(tuple)
        this.tuple = tuple
    }

    async refresh(): Promise<ActivePersistentTuple> {
        const tuple = await this.store.readActiveTuple()
        this.install(tuple)
        return tuple
    }

    current(): ActivePersistentTuple {
        if (!this.tuple) throw new Error('Active payload root is not installed')
        return this.tuple
    }

    async getActiveRoot(): Promise<BlobStorageRoot> {
        return rootFromTuple(this.current())
    }

    getActiveColdRoot(): BlobStorageRoot {
        return rootFromTuple(this.current())
    }
}
