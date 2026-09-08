import type { Database } from './database.svelte'
import type {
    AssetAlias,
    AssetAliasIdentity,
    AssetAliasKind,
    AssetAliasListQuery,
    AssetRepositoryMigrationInput,
    AssetOwnerLocator,
    ColdAlias,
    ColdPayloadMigrationInput,
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
        readColdPayloadAuthority: () => store.readColdPayloadAuthority(),
        readColdAlias: (key: string) => store.readColdAlias(key),
        listColdAliases: () => store.listColdAliases(),
        commitAssetAlias: (alias: AssetAlias, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.commitAssetAlias(alias, expectedRevision)),
        deleteAssetAlias: (identity: AssetAliasIdentity, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.deleteAssetAlias(identity, expectedRevision)),
        activateAssetRepositoryMigration: (input: AssetRepositoryMigrationInput) =>
            gate.runTransition(() => store.activateAssetRepositoryMigration(input)),
        commitColdAlias: (alias: ColdAlias, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.commitColdAlias(alias, expectedRevision)),
        deleteColdAlias: (key: string, expectedRevision: DataRevision) =>
            gate.runWrite(() => store.deleteColdAlias(key, expectedRevision)),
        activateColdPayloadMigration: (input: ColdPayloadMigrationInput) =>
            gate.runTransition(() => store.activateColdPayloadMigration(input)),
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
