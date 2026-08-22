import { inlayTokenRegex } from '../util/inlayTokens'
import {
    listCharacterResources,
    listColdDataKeysFromDb,
    listDatabaseRootResources,
} from '../process/coldstorageData'
import type { Database, character, groupChat } from './database.svelte'
import type { MigrationReferenceGraph } from './losslessMigrationOrchestrator'

function addAsset(resources: Set<string>, value: string): void {
    if (value.startsWith('assets/') && value.length > 'assets/'.length) resources.add(value)
}

function collectInlays(value: unknown, output: Set<string>, seen: WeakSet<object>): void {
    if (typeof value === 'string') {
        const regex = new RegExp(inlayTokenRegex.source, inlayTokenRegex.flags)
        for (let match = regex.exec(value); match; match = regex.exec(value)) output.add(match[2])
        return
    }
    if (!value || typeof value !== 'object' || seen.has(value)) return
    seen.add(value)
    if (Array.isArray(value)) {
        for (const item of value) collectInlays(item, output, seen)
        return
    }
    for (const item of Object.values(value)) collectInlays(item, output, seen)
}

function coldCharacter(value: unknown): character | groupChat | null {
    if (!value || typeof value !== 'object' || !('character' in value)) return null
    const candidate = (value as { character?: unknown }).character
    return candidate && typeof candidate === 'object' ? candidate as character | groupChat : null
}

export function collectMigrationReferenceGraph(
    database: Database,
    coldValues: ReadonlyMap<string, unknown>,
): MigrationReferenceGraph {
    const assets = new Set<string>()
    const inlays = new Set<string>()
    for (const value of listDatabaseRootResources(database)) addAsset(assets, value)
    for (const value of database.characters ?? []) {
        if (!value) continue
        for (const resource of listCharacterResources(value)) addAsset(assets, resource)
    }
    for (const value of coldValues.values()) {
        const candidate = coldCharacter(value)
        if (candidate) {
            for (const resource of listCharacterResources(candidate)) addAsset(assets, resource)
        }
    }
    collectInlays(database, inlays, new WeakSet())
    for (const value of coldValues.values()) collectInlays(value, inlays, new WeakSet())
    return {
        assets: [...assets].sort(),
        inlays: [...inlays].sort(),
        cold: [...new Set(listColdDataKeysFromDb(database))].sort(),
    }
}
