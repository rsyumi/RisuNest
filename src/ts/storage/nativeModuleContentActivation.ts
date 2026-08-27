import { alertConfirm } from '../alert'
import { language } from '../../lang'
import { v4 } from 'uuid'

import type { RisuModule } from '../process/modules'
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
}

const productionDependencies: NativeModuleContentActivationDependencies = {
    confirmLowLevelAccess: () => alertConfirm(language.lowLevelAccessConfirm),
    createId: v4,
    append: appendPersistentRootModule,
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
    const content = requireRisum(prepared)
    const module = structuredClone(content.metadata) as unknown as RisuModule
    if (module.lowLevelAccess && !await dependencies.confirmLowLevelAccess()) return null

    const moduleId = dependencies.createId()
    module.id = moduleId
    if (Object.hasOwn(module, 'assets')) {
        if (!Array.isArray(module.assets) || module.assets.length !== content.assets.length) {
            throw new TypeError('Prepared RISUM asset metadata does not match its descriptors')
        }
        module.assets = module.assets.map((tuple, position) => {
            if (!Array.isArray(tuple) || tuple.length < 3) {
                throw new TypeError(`Prepared RISUM asset tuple ${position} is invalid`)
            }
            const retained = structuredClone(tuple)
            retained[1] = content.assets[position].logicalId
            return retained
        })
    }

    const assetAliasesByKey = new Map<string, Extract<AssetAlias, { kind: 'asset' }>>()
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
        if (previous && (previous.objectHash !== alias.objectHash || previous.size !== alias.size)) {
            throw new TypeError(`Prepared RISUM alias ${alias.key} has conflicting payloads`)
        }
        if (!previous) assetAliasesByKey.set(alias.key, alias)
    }
    const assetAliases = [...assetAliasesByKey.values()]
    if (!lifecycle.sealPreparedContent) {
        throw new TypeError('Prepared RISUM content cannot seal its native roots')
    }
    signal?.throwIfAborted()
    await lifecycle.sealPreparedContent()
    if (signal?.aborted) {
        await lifecycle.abortPreparedContent?.()
        signal.throwIfAborted()
    }
    try {
        await dependencies.append({ module, assetAliases, ownerHead: content.ownerHead }, signal)
    }
    catch (error) {
        if (
            error instanceof PersistentRootModuleAppendRejectedError
            || (signal?.aborted && error === signal.reason)
        ) {
            await lifecycle.abortPreparedContent?.()
        }
        throw error
    }
    return { moduleId }
}
