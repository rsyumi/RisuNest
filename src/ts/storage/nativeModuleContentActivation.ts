import { alertConfirm } from '../alert'
import { language } from '../../lang'
import { v4 } from 'uuid'

import type { RisuModule } from '../process/modules'
import { markModuleImport } from '../contentUpdate/merge'
import { assertModuleMCPImportAllowed } from '../process/mcp/moduleImport'
import type { AssetAlias } from './persistentDataStore'
import type {
    PreparedNativeContent,
    PreparedNativeContentActivationLifecycle,
    PreparedNativeRisumContent,
    PreparedRisumOwnerHead,
} from './nativeFileJobs'
import { appendPersistentRootModule } from './persistentDataRuntime.svelte'
import { PersistentRootModuleAppendRejectedError } from './saveCoordinator'

export interface PreparedRootModuleAppend {
    module: RisuModule
    assetAliases: Extract<AssetAlias, { kind: 'asset' }>[]
    ownerHead: PreparedRisumOwnerHead
}

export interface NativeModuleContentActivationDependencies {
    confirmLowLevelAccess(): Promise<boolean>
    createId(): string
    append(input: PreparedRootModuleAppend, signal?: AbortSignal): Promise<void>
    /**
     * Adds the module through `commit`, or offers to update an installed one instead.
     * Resolves the id the import ended as, or null when the user cancelled it.
     */
    commitImport?(
        module: RisuModule,
        commit: (value: RisuModule) => Promise<unknown>,
        signal?: AbortSignal,
    ): Promise<string | null>
}

const productionDependencies: NativeModuleContentActivationDependencies = {
    confirmLowLevelAccess: () => alertConfirm(language.lowLevelAccessConfirm),
    createId: v4,
    append: appendPersistentRootModule,
    commitImport: async (module, commit, signal) => {
        const { commitImportedModule } = await import('../contentUpdate/flow.svelte')
        return commitImportedModule(module, commit, signal)
    },
}

async function commitModule(
    dependencies: NativeModuleContentActivationDependencies,
    module: RisuModule,
    commit: (value: RisuModule) => Promise<void>,
    signal: AbortSignal | undefined,
): Promise<string | null> {
    if (!dependencies.commitImport) {
        await commit(module)
        return module.id
    }
    return dependencies.commitImport(module, commit, signal)
}

function requireRisum(content: PreparedNativeContent): PreparedNativeRisumContent {
    if (content.format !== 'risu-module') throw new TypeError('Prepared content is not a RISUM module')
    return content as PreparedNativeRisumContent
}

export async function activatePreparedNativeModuleContent(
    prepared: PreparedNativeContent,
    lifecycle: PreparedNativeContentActivationLifecycle,
    dependencies: NativeModuleContentActivationDependencies = productionDependencies,
    signal?: AbortSignal,
): Promise<{ moduleId: string } | null> {
    if (prepared.format !== 'risu-module') {
        const [
            { convertCharacterToModule },
            {
                mapPreparedNativeContent,
                preparedAliases,
                prepareAdditionalAssetOwnerHead,
            },
        ] = await Promise.all([
            import('../interchangeability'),
            import('./nativeCharacterContentActivation'),
        ])
        const character = await mapPreparedNativeContent(
            prepared,
            undefined,
            signal,
            lifecycle,
        )
        if (!character) return null
        const module = convertCharacterToModule(character)
        module.id = dependencies.createId()
        const assetAliases = preparedAliases(prepared)
        let moduleId: string | null
        try {
            moduleId = await commitModule(dependencies, module, async (value) => {
                signal?.throwIfAborted()
                const head = await prepareAdditionalAssetOwnerHead(
                    character,
                    assetAliases,
                    lifecycle.prepareOwnerManifestAndSeal,
                )
                signal?.throwIfAborted()
                await dependencies.append(
                    {
                        module: value,
                        assetAliases,
                        ownerHead: {
                            present: true,
                            manifestHash: head.manifestHash!,
                            entryCount: head.entryCount,
                        },
                    },
                    signal,
                )
            }, signal)
        } catch (error) {
            if (
                error instanceof PersistentRootModuleAppendRejectedError ||
                (signal?.aborted && error === signal.reason)
            ) {
                try {
                    await lifecycle.abortPreparedContent?.()
                } catch {}
            }
            throw error
        }
        return moduleId ? { moduleId } : null
    }
    const content = requireRisum(prepared)
    const module = structuredClone(content.metadata) as unknown as RisuModule
    assertModuleMCPImportAllowed(module)
    if (module.lowLevelAccess && !(await dependencies.confirmLowLevelAccess()))
        return null

    markModuleImport(module)
    module.id = dependencies.createId()
    delete module.mcp
    if (Object.hasOwn(module, 'assets')) {
        if (
            !Array.isArray(module.assets) ||
            module.assets.length !== content.assets.length
        ) {
            throw new TypeError(
                'Prepared RISUM asset metadata does not match its descriptors',
            )
        }
        module.assets = module.assets.map((tuple, position) => {
            if (!Array.isArray(tuple) || tuple.length < 3) {
                throw new TypeError(
                    `Prepared RISUM asset tuple ${position} is invalid`,
                )
            }
            const retained = structuredClone(tuple)
            retained[1] = content.assets[position].logicalId
            return retained
        })
    }

    const assetAliasesByKey = new Map<
        string,
        Extract<AssetAlias, { kind: 'asset' }>
    >()
    for (const asset of content.assets) {
        const alias: Extract<AssetAlias, { kind: 'asset' }> = {
            kind: 'asset',
            key: asset.logicalId,
            objectHash: asset.objectHash,
            size: asset.byteSize,
            mime: asset.mime,
            name: asset.name,
            ext: asset.ext,
        }
        const previous = assetAliasesByKey.get(alias.key)
        if (
            previous &&
            (previous.objectHash !== alias.objectHash ||
                previous.size !== alias.size)
        ) {
            throw new TypeError(
                `Prepared RISUM alias ${alias.key} has conflicting payloads`,
            )
        }
        if (!previous) assetAliasesByKey.set(alias.key, alias)
    }
    const assetAliases = [...assetAliasesByKey.values()]
    const sealPreparedContent = lifecycle.sealPreparedContent
    if (!sealPreparedContent) {
        throw new TypeError(
            'Prepared RISUM content cannot seal its native roots',
        )
    }
    let moduleId: string | null
    let sealed = false
    try {
        moduleId = await commitModule(dependencies, module, async (value) => {
            signal?.throwIfAborted()
            await sealPreparedContent()
            sealed = true
            // A cancel after sealing is undone below, with the rejected appends.
            signal?.throwIfAborted()
            await dependencies.append(
                { module: value, assetAliases, ownerHead: content.ownerHead },
                signal,
            )
        }, signal)
    } catch (error) {
        if (sealed && (
            error instanceof PersistentRootModuleAppendRejectedError ||
            (signal?.aborted && error === signal.reason)
        )) {
            try {
                await lifecycle.abortPreparedContent?.()
            } catch {}
        }
        throw error
    }
    return moduleId ? { moduleId } : null
}
