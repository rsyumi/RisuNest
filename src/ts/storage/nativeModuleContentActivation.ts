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

export interface PreparedRootModuleAppend {
    module: RisuModule
    assetAliases: Extract<AssetAlias, { kind: 'asset' }>[]
    ownerHead: PreparedRisumOwnerHead
}

export interface NativeModuleContentActivationDependencies {
    confirmLowLevelAccess(): Promise<boolean>
    createId(): string
    append(input: PreparedRootModuleAppend): Promise<void>
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
            return [tuple[0], content.assets[position].logicalId, tuple[2]]
        })
    }

    const assetAliases: Extract<AssetAlias, { kind: 'asset' }>[] = content.assets.map((asset) => ({
        kind: 'asset',
        key: asset.logicalId,
        objectHash: asset.objectHash,
        size: asset.byteSize,
        mime: asset.mime,
        name: asset.name,
        ext: asset.ext,
    }))
    if (!lifecycle.sealPreparedContent) {
        throw new TypeError('Prepared RISUM content cannot seal its native roots')
    }
    await lifecycle.sealPreparedContent()
    await dependencies.append({ module, assetAliases, ownerHead: content.ownerHead })
    return { moduleId }
}
