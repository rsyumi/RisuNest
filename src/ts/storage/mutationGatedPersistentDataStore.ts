import type { Database } from './database.svelte'
import type {
    ActivePersistentTuple,
    CharacterPage,
    CharacterQuery,
    ConversationPage,
    ConversationQuery,
    ConversationWindow,
    ConversationWindowQuery,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
    PreparedPersistentReplacement,
    PreparedReplacementActivation,
    Versioned,
    WorkingSetCommit,
    CharacterDetail,
} from './persistentDataStore'
import type { StorageMutationGate } from './storageMutationGate'

export function createMutationGatedPersistentDataStore(
    store: PersistentDataStore,
    gate: StorageMutationGate,
): PersistentDataStore {
    return {
        open: () => store.open(),
        readRoot: () => store.readRoot(),
        queryCharacters: (input: CharacterQuery): Promise<CharacterPage> =>
            store.queryCharacters(input),
        readCharacter: (id: string): Promise<Versioned<CharacterDetail> | null> =>
            store.readCharacter(id),
        queryConversations: (input: ConversationQuery): Promise<ConversationPage> =>
            store.queryConversations(input),
        readConversation: (characterId, conversationId) =>
            store.readConversation(characterId, conversationId),
        readConversationWindow: (
            input: ConversationWindowQuery,
        ): Promise<Versioned<ConversationWindow> | null> => store.readConversationWindow(input),
        commit: (input: WorkingSetCommit) => gate.runWrite(() => store.commit(input)),
        replaceFromDatabase: (database: Database, expectedRevision?: DataRevision) =>
            gate.runWrite(() => store.replaceFromDatabase(database, expectedRevision)),
        prepareReplacement: (database, manifestHash, payloadGeneration) =>
            store.prepareReplacement(database, manifestHash, payloadGeneration),
        activatePreparedReplacement: (input: PreparedReplacementActivation) =>
            store.activatePreparedReplacement(input),
        discardPreparedReplacement: (prepared: PreparedPersistentReplacement) =>
            store.discardPreparedReplacement(prepared),
        listPreparedReplacements: () => store.listPreparedReplacements(),
        readActivePayloadGeneration: () => store.readActivePayloadGeneration(),
        readActiveTuple: (): Promise<ActivePersistentTuple> => store.readActiveTuple(),
        materializeDatabase: (revision?: DataRevision) => store.materializeDatabase(revision),
        acquireRevision: (revision: DataRevision): Promise<PersistentRevisionLease> =>
            store.acquireRevision(revision),
    }
}
