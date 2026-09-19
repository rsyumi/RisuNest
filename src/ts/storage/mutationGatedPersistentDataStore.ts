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
        readCharacterSummary: (id: string) => store.readCharacterSummary(id),
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
        readPluginStorage: (owner: string, key: string) => store.readPluginStorage(owner, key),
        listPluginStorage: () => store.listPluginStorage(),
        ...(store.commitWorkingSetChangeCursor === undefined
            ? {}
            : {
                commitWorkingSetChangeCursor: (revision: DataRevision) =>
                    store.commitWorkingSetChangeCursor!(revision),
            }),
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
        archivePreview: (characterId: string) => store.archivePreview(characterId),
        archiveCharacter: (
            characterId: string,
            expectedRevision: DataRevision,
            signal?: AbortSignal,
        ) => gate.runWrite(() => store.archiveCharacter(characterId, expectedRevision, signal)),
        restoreCharacter: (
            characterId: string,
            expectedRevision: DataRevision,
            signal?: AbortSignal,
        ) => gate.runWrite(() => store.restoreCharacter(characterId, expectedRevision, signal)),
        replaceFromDatabase: (...args: Parameters<PersistentDataStore['replaceFromDatabase']>) =>
            gate.runTransition(() => store.replaceFromDatabase(...args)),
        materializeDatabase: (revision?: DataRevision) => store.materializeDatabase(revision),
        acquireRevision: (revision: DataRevision): Promise<PersistentRevisionLease> =>
            store.acquireRevision(revision),
    }
}
