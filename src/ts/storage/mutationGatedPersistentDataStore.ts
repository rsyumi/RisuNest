import type { Database } from './database.svelte'
import type {
    AssetAlias,
    AssetOwnerLocator,
    CharacterPage,
    CharacterQuery,
    ConversationPage,
    ConversationQuery,
    ConversationWindow,
    ConversationWindowQuery,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
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
        queryPresets: () => store.queryPresets(),
        readPreset: (id: string) => store.readPreset(id),
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
        queryPluginStorage: () => store.queryPluginStorage(),
        readPluginStorage: (key: string) => store.readPluginStorage(key),
        readAssetAlias: (key: string) => store.readAssetAlias(key),
        readAssetOwnerHead: (owner: AssetOwnerLocator) => store.readAssetOwnerHead(owner),
        commitAssetAlias: (alias: AssetAlias, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.commitAssetAlias(alias, expectedRevision)),
        commit: (input: WorkingSetCommit) => gate.runWrite(() => store.commit(input)),
        replaceFromDatabase: (
            database: Database,
            expectedRevision?: DataRevision,
            assetAliases?: AssetAlias[],
        ) => gate.runWrite(() =>
            store.replaceFromDatabase(database, expectedRevision, assetAliases)),
        materializeDatabase: (revision?: DataRevision) => store.materializeDatabase(revision),
        acquireRevision: (revision: DataRevision): Promise<PersistentRevisionLease> =>
            store.acquireRevision(revision),
    }
}
