import type { Database } from './database.svelte'
import type {
    AssetAlias,
    AssetAliasIdentity,
    AssetAliasKind,
    AssetAliasListQuery,
    AssetRepositoryMigrationInput,
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
        readConversationMetadata: (characterId, conversationId) =>
            store.readConversationMetadata(characterId, conversationId),
        readConversationWindow: (
            input: ConversationWindowQuery,
        ): Promise<Versioned<ConversationWindow> | null> => store.readConversationWindow(input),
        queryPluginStorage: () => store.queryPluginStorage(),
        readPluginStorage: (key: string) => store.readPluginStorage(key),
        readAssetAlias: (identity: AssetAliasIdentity) => store.readAssetAlias(identity),
        readAssetAliasesByKeys: (kind: AssetAliasKind, keys: string[]) =>
            store.readAssetAliasesByKeys(kind, keys),
        listAssetAliases: (input: AssetAliasListQuery) => store.listAssetAliases(input),
        readAssetRepositoryAuthority: () => store.readAssetRepositoryAuthority(),
        readAssetOwnerHead: (owner: AssetOwnerLocator) => store.readAssetOwnerHead(owner),
        commitAssetAlias: (alias: AssetAlias, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.commitAssetAlias(alias, expectedRevision)),
        deleteAssetAlias: (identity: AssetAliasIdentity, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.deleteAssetAlias(identity, expectedRevision)),
        activateAssetRepositoryMigration: (input: AssetRepositoryMigrationInput) =>
            gate.runTransition(() => store.activateAssetRepositoryMigration(input)),
        commit: (input: WorkingSetCommit) => gate.runWrite(() => store.commit(input)),
        replaceFromDatabase: (
            database: Database,
            expectedRevision?: DataRevision,
            assetAliases?: AssetAlias[],
        ) => gate.runTransition(() =>
            store.replaceFromDatabase(database, expectedRevision, assetAliases)),
        materializeDatabase: (revision?: DataRevision) => store.materializeDatabase(revision),
        acquireRevision: (revision: DataRevision): Promise<PersistentRevisionLease> =>
            store.acquireRevision(revision),
    }
}
