import type { GeneratingConversation } from '../storage/persistentDataStore'
import type { PersistentUnitIntentProjectionReceipt } from '../storage/persistentDataRuntime'

// A generation's request is pending between prompt building and the output message.
// Plugin patches of plugin-owned fields may commit only then, and the generation waits
// for the patches in flight before it applies the response.

const key = (target: GeneratingConversation) => JSON.stringify([target.characterId, target.conversationId])
const openPhases = new Map<string, number>()
const patchesInFlight = new Map<string, Set<Promise<void>>>()
interface PublicWriteObserver {
    prepare(): (() => boolean) | null
    prepareMetadataPatch?(expectedRevision: number): ((durableRevision: number) => Promise<void>) | null
    invalidate(): void
    adopt(receipt: PersistentUnitIntentProjectionReceipt): Promise<void>
}
const publicWriteObservers = new Map<string, Set<PublicWriteObserver>>()

export function isGenerationRequestPhaseOpen(target: GeneratingConversation): boolean {
    return openPhases.has(key(target))
}

/** Opens the request phase; the returned close stops admitting patches and waits for those in flight. */
export function openGenerationRequestPhase(target: GeneratingConversation, publicWrites?: PublicWriteObserver): () => Promise<void> {
    const identity = key(target)
    openPhases.set(identity, (openPhases.get(identity) ?? 0) + 1)
    const observers = publicWriteObservers.get(identity) ?? new Set<PublicWriteObserver>()
    if (publicWrites) {
        observers.add(publicWrites)
        publicWriteObservers.set(identity, observers)
    }
    let closed = false
    return async () => {
        if (!closed) {
            closed = true
            const count = openPhases.get(identity)! - 1
            if (count > 0) openPhases.set(identity, count)
            else openPhases.delete(identity)
        }
        await settleConversationPatches(target)
        if (publicWrites) observers.delete(publicWrites)
        if (!observers.size && publicWriteObservers.get(identity) === observers) publicWriteObservers.delete(identity)
    }
}

/** A request may reload its input window after an admitted plugin-field-only patch. */
export function prepareGenerationMetadataPatch(target: GeneratingConversation, expectedRevision: number) {
    const accepted = [...publicWriteObservers.get(key(target)) ?? []].flatMap(observer => {
        const adopt = observer.prepareMetadataPatch?.(expectedRevision)
        return adopt ? [adopt] : []
    })
    return accepted.length ? async (durableRevision: number) => {
        for (const adopt of accepted) await adopt(durableRevision)
    } : null
}

/** Public setters retain their write semantics and let an admitted request adopt their result. */
export function captureGenerationPublicWrite(target: GeneratingConversation | null) {
    if (!target || !isGenerationRequestPhaseOpen(target)) return null
    const candidates = [...publicWriteObservers.get(key(target)) ?? []]
    const release = trackConversationPatch(target)
    let accepted: Array<{ observer: PublicWriteObserver; isCurrent: () => boolean }> = []
    let projection: PersistentUnitIntentProjectionReceipt | null = null
    let finished = false
    return {
        prepare(actual: GeneratingConversation) {
            if (key(actual) === key(target)) accepted = candidates.flatMap((observer) => {
                const isCurrent = observer.prepare()
                return isCurrent ? [{ observer, isCurrent }] : []
            })
        },
        beforeProjection() {
            accepted = accepted.filter(({ observer, isCurrent }) => {
                if (isCurrent()) return true
                observer.invalidate()
                return false
            })
        },
        afterProjection(receipt: PersistentUnitIntentProjectionReceipt) {
            if (receipt.target && key(receipt.target) === key(target)) projection = receipt
        },
        async finish(committed: boolean) {
            if (finished) return
            finished = true
            try {
                if (committed && projection) for (const { observer } of accepted) await observer.adopt(projection)
            } finally {
                release()
            }
        },
    }
}

export function trackConversationPatch(target: GeneratingConversation): () => void {
    const identity = key(target)
    let finish!: () => void
    const settled = new Promise<void>((resolve) => { finish = resolve })
    const patches = patchesInFlight.get(identity) ?? new Set<Promise<void>>()
    patches.add(settled)
    patchesInFlight.set(identity, patches)
    return () => {
        finish()
        patches.delete(settled)
        if (!patches.size && patchesInFlight.get(identity) === patches) patchesInFlight.delete(identity)
    }
}

export async function settleConversationPatches(target: GeneratingConversation): Promise<void> {
    const identity = key(target)
    for (let patches = patchesInFlight.get(identity); patches?.size; patches = patchesInFlight.get(identity)) {
        await Promise.all([...patches])
    }
}
