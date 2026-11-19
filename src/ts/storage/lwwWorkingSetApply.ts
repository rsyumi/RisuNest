import { sharedRootFields } from './persistentRootFields'
import { createCatalogCharacterStub, createCatalogPresetWorkingSet, isCatalogPresetLoaded, isCatalogPresetWorkingSet, placeCatalogPresetBody } from './workingSetCatalog'
import { createConversationSummaryStub, createConversationSummaryFromMetadata } from './conversationResidency'
import type { Chat, Database, botPreset, character } from './database.svelte'
import type { PersistentRevisionReader, PersistentRoot } from './persistentDataStore'
import { canonicalClone, canonicalJson, clonePersistentRootFields } from './saveCoordinatorHelpers'
import { captureMaterializedCharacter, CHARACTER_SHARED_FIELDS, CONVERSATION_SHARED_FIELDS } from './persistentUnitCapture'
import { isWorkingSetCharacterStub, patchCatalogCharacterField } from './workingSetCatalog'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'

type CompleteCharacter = character

export interface LwwWorkingSetBaseline {
    root: PersistentRoot
    presets: botPreset[] | null
    presetRecords: botPreset[]
    characters: CompleteCharacter[]
}

export function captureLwwWorkingSetBaseline(database: Database, root: PersistentRoot, presets: botPreset[] | null, materialized?: CompleteCharacter[], presetRecords: readonly botPreset[] = []): LwwWorkingSetBaseline {
    return { root: clonePersistentRootFields(root), presets: presets && canonicalClone(presets),
        presetRecords: canonicalClone([...presetRecords]),
        characters: materialized ?? database.characters.filter((value) => !isWorkingSetCharacterStub(value)).map(captureMaterializedCharacter) }
}

function patchField(live: object, baseline: object, remote: object, field: string): void {
    const current = live as Record<string, unknown>, before = baseline as Record<string, unknown>, next = remote as Record<string, unknown>
    const unchanged = canonicalJson({ value: current[field] }) === canonicalJson({ value: before[field] })
    const write = (value: Record<string, unknown>) => {
        if (!Object.hasOwn(next, field)) delete value[field]
        // Only assignment makes a state proxy report a new key to cached captures.
        else if (field === '__proto__') defineOwnEnumerableProperty(value, field, canonicalClone(next[field]))
        else value[field] = canonicalClone(next[field])
    }
    if (unchanged) write(current)
    write(before)
}

export async function applyLwwWorkingSetUnits(
    database: Database, baseline: LwwWorkingSetBaseline, store: PersistentRevisionReader, affectedKeys: readonly string[], liveIndex?: ReadonlyMap<string, CompleteCharacter>, allowLocalFields = false, beforeProjection?: () => void, afterProjection?: () => void,
): Promise<LwwWorkingSetBaseline> {
    const charactersById = liveIndex ?? new Map(database.characters.map((value) => [value.chaId,value]))
    // A stub holds only catalog fields, so no baseline describes it.
    const baselineById = new Map(baseline.characters.filter((value) => {
        const live = charactersById.get(value.chaId)
        return !live || !isWorkingSetCharacterStub(live)
    }).map((value) => [value.chaId,value]))
    const keys = affectedKeys.map((key) => JSON.parse(key) as string[])
        .filter(([kind, , field, metadataField]) => allowLocalFields || (kind !== 'character' || CHARACTER_SHARED_FIELDS.has(field)) && (kind !== 'conversation' || CONVERSATION_SHARED_FIELDS.has(metadataField)))
        .sort((a, b) => Number(a[0] === 'root' && ['botPresetsId', 'selectedPersona'].includes(a[1])) - Number(b[0] === 'root' && ['botPresetsId', 'selectedPersona'].includes(b[1])))
    const root = keys.some(([kind]) => ['root', 'variable', 'toggle', 'record', 'order', 'persona', 'preset-protected', 'exists'].includes(kind))
        ? (await store.readRoot()).value : null
    const details = new Map<string, CompleteCharacter | null>()
    for (const [kind, id] of keys) if (kind === 'character') {
        if (!details.has(id)) {
            const value = await store.readCharacter(id)
            details.set(id, value ? { ...value.value, chats: [] } as CompleteCharacter : null)
        }
    }
    const presets = new Map<string, Awaited<ReturnType<PersistentRevisionReader['readPreset']>>>()
    const metadata = new Map<string, Awaited<ReturnType<PersistentRevisionReader['readConversationMetadata']>>>()
    const conversations = new Map<string, Awaited<ReturnType<PersistentRevisionReader['readConversation']>>>()
    const orders = new Map<string, string[]>()
    const summaries = new Map<string, Awaited<ReturnType<PersistentRevisionReader['readCharacterSummary']>>>()
    let presetCatalog: Awaited<ReturnType<PersistentRevisionReader['queryPresets']>> | undefined
    const pair = (characterId: string, conversationId: string) => JSON.stringify([characterId, conversationId])
    const catalogPresets = isCatalogPresetWorkingSet(database.botPresets)
    // Places a preset's stored body in place of its catalog summary.
    const placePresetBody = (index: number, value: botPreset) => {
        const recorded = baseline.presetRecords.findIndex((record) => record['id'] === value.id)
        if (recorded >= 0) baseline.presetRecords.splice(recorded, 1)
        baseline.presetRecords.push(canonicalClone(value))
        placeCatalogPresetBody(database.botPresets, index, canonicalClone(value))
    }
    // A catalog working set holds its selection's body, also when the selection fell back to another preset.
    const placeSelectedBody = () => {
        const id = database.botPresets[database.botPresetsId]?.['id']
        const preset = typeof id === 'string' ? presets.get(id) : undefined
        if (preset && !isCatalogPresetLoaded(database.botPresets, id as string)) placePresetBody(database.botPresetsId, preset.value)
    }
    const readPreset = async (id: string) => {
        if (presets.has(id)) return
        const live = database.botPresets.find((value) => value?.['id'] === id)
        if (live && !baseline.presets?.some((value) => value['id'] === id) && !baseline.presetRecords.some((value) => value['id'] === id)) baseline.presetRecords.push(canonicalClone(live))
        presets.set(id, await store.readPreset(id))
    }
    for (const [kind, id, field, metadataField] of keys) {
        if (kind === 'preset' && database.botPresets.some((value) => value?.['id'] === id)) await readPreset(id)
        if (!isCatalogPresetWorkingSet(database.botPresets) && (kind === 'preset' || kind === 'exists' && id === 'preset')) await readPreset(kind === 'preset' ? id : field)
        if (kind === 'root' && id === 'botPresetsId') {
            const received = typeof root?.botPresetsId === 'string' ? root.botPresetsId : undefined
            if (received) await readPreset(received)
            if (received && presets.get(received)) presetCatalog ??= await store.queryPresets()
            else if (catalogPresets) {
                // An index names the preset at that position; a selection naming a preset this device does not
                // hold, or none, falls back to the first. The selection key runs last, so a received order or
                // removal has already read that catalog.
                const index = typeof root?.botPresetsId === 'number' ? root.botPresetsId : 0
                const selected = presetCatalog ? presetCatalog.items.find((item) => item.configuredIndex === index)?.id : database.botPresets[index]?.['id']
                if (typeof selected === 'string') await readPreset(selected)
            }
        }
        if ((kind === 'order' && id === 'presets') || (kind === 'exists' && id === 'preset')) {
            presetCatalog ??= await store.queryPresets()
            // A removed selection falls back to the first preset.
            const selectedId = database.botPresets[database.botPresetsId]?.['id']
            if (catalogPresets && !presetCatalog.items.some((item) => item.id === selectedId) && presetCatalog.items[0]) await readPreset(presetCatalog.items[0].id)
        }
        if (kind === 'conversation' || kind === 'messages') {
            const live = charactersById.get(id)?.chats.find((value) => value.id === field)
            const before = baselineById.get(id)?.chats.find((value) => value.id === field)
            if (live && before) {
                const key = pair(id, field)
                if (kind === 'messages') {
                    if (Object.hasOwn(before, 'message') && Object.prototype.propertyIsEnumerable.call(live, 'message') && !conversations.has(key)) conversations.set(key, await store.readConversation(id, field))
                } else if (!metadata.has(key)) metadata.set(key, await store.readConversationMetadata(id, field))
            }
        }
        if (kind === 'order' && id === 'conversations' && charactersById.has(field) && baselineById.has(field) && !orders.has(field)) {
            const ids: string[] = []
            let cursor: string | undefined
            do { const page = await store.queryConversations({ characterId: field, order: 'configured', limit: 128, cursor }); ids.push(...page.items.map((item) => item.id)); cursor = page.nextCursor } while (cursor)
            orders.set(field, ids)
            if (!details.has(field)) { const value = await store.readCharacter(field); details.set(field, value ? { ...value.value, chats: [] } as CompleteCharacter : null) }
        }
        if (kind === 'exists' && id === 'character' && !summaries.has(field)) summaries.set(field, await store.readCharacterSummary(field))
        if (kind === 'exists' && id === 'conversation' && !metadata.has(pair(field, metadataField))) metadata.set(pair(field, metadataField), await store.readConversationMetadata(field, metadataField))
    }
    beforeProjection?.()
    for (const [kind, id, field, metadataField] of keys) {
        if (kind === 'root' && root && (allowLocalFields || sharedRootFields.has(id))) {
            if (id === 'botPresetsId' && typeof root.botPresetsId === 'string') {
                const preset = presets.get(root.botPresetsId)
                if (preset) {
                    const catalog = presetCatalog!
                    const index = catalog.items.find((item) => item.id === root.botPresetsId)?.configuredIndex
                    if (index !== undefined) {
                        const live = database.botPresets.find((value) => value?.['id'] === root.botPresetsId)
                        const before = baseline.presets?.find((value) => value['id'] === root.botPresetsId) ?? baseline.presetRecords.find((value) => value['id'] === root.botPresetsId)
                        if (live && before && isCatalogPresetLoaded(database.botPresets, root.botPresetsId)) {
                            for (const field of new Set([...Object.keys(before), ...Object.keys(preset.value)])) patchField(live, before, preset.value, field)
                        } else placePresetBody(index, preset.value)
                    }
                }
            }
            const remote = { ...root } as unknown as Record<string, unknown>
            // A removed selection selects the first record.
            if ((id === 'botPresetsId' || id === 'selectedPersona') && !Object.hasOwn(remote, id)) remote[id] = 0
            if (id === 'botPresetsId' && typeof remote[id] === 'string') remote[id] = Math.max(0, database.botPresets.findIndex((value) => value?.['id'] === remote[id]))
            if (id === 'selectedPersona' && typeof remote[id] === 'string') remote[id] = Math.max(0, database.personas?.findIndex((value) => value.id === remote[id]) ?? 0)
            const before = { ...baseline.root } as unknown as Record<string, unknown>
            if (id === 'botPresetsId' && typeof before[id] === 'string') before[id] = Math.max(0, database.botPresets.findIndex((value) => value?.['id'] === before[id]))
            if (id === 'selectedPersona' && typeof before[id] === 'string') before[id] = Math.max(0, database.personas?.findIndex((value) => value.id === before[id]) ?? 0)
            patchField(database, before, remote, id)
            if (id === 'botPresetsId') placeSelectedBody()
            if (Object.hasOwn(root, id)) (baseline.root as unknown as Record<string, unknown>)[id] = canonicalClone((root as unknown as Record<string, unknown>)[id])
            else delete (baseline.root as unknown as Record<string, unknown>)[id]
            // The record a received index or unresolved selection lands on is not a local selection to save.
            if (id === 'botPresetsId' || id === 'selectedPersona') {
                const records: readonly ({ id?: unknown } | undefined)[] = (id === 'botPresetsId' ? database.botPresets : database.personas) ?? []
                const received = root[id], index = database[id]
                if ((typeof received !== 'string' || !records.some((value) => value?.id === received)) && index === remote[id]) {
                    const fallbackId = records[index]?.id
                    const baselineRoot = baseline.root as unknown as Record<string, unknown>
                    baselineRoot[id] = typeof fallbackId === 'string' ? fallbackId : index
                }
            }
        } else if (kind === 'character') {
            const live = charactersById.get(id)
            const before = baselineById.get(id)
            const remote = details.get(id)
            if (!live || !remote) continue
            // A stub takes its catalog fields from the stored record.
            if (isWorkingSetCharacterStub(live)) {
                patchCatalogCharacterField(live, remote, field)
                continue
            }
            if (!before) continue
            for (const key of [field]) {
                if (key !== 'statics' || allowLocalFields) { patchField(live, before, remote, key); continue }
                const sharedStatics = (value: unknown) => value && typeof value === 'object' ? Object.fromEntries(Object.entries(value).filter(([key]) => key !== 'messages')) : value
                const liveView = {statics:sharedStatics(live['statics'])}, beforeView = {statics:sharedStatics(before['statics'])}
                patchField(liveView, beforeView, {statics:sharedStatics(remote['statics'])}, 'statics')
                for (const [target, view] of [[live,liveView],[before,beforeView]] as const) {
                    const local = target['statics'] as Record<string, unknown> | undefined
                    if (view.statics === undefined && !local?.messages) delete target['statics']
                    else target['statics'] = {...view.statics as object, ...(local && Object.hasOwn(local, 'messages') ? {messages:local.messages} : {})}
                }
            }
        } else if (kind === 'conversation' || kind === 'messages') {
            const liveCharacter = charactersById.get(id)
            const beforeCharacter = baselineById.get(id)
            const live = liveCharacter?.chats.find((value) => value.id === field)
            const before = beforeCharacter?.chats.find((value) => value.id === field)
            if (!live || !before) continue
            if (kind === 'messages') {
                if (!Object.hasOwn(before, 'message') || !Object.prototype.propertyIsEnumerable.call(live, 'message')) continue
                const remote = conversations.get(pair(id, field))
                if (remote) patchField(live, before, remote.value, 'message')
            } else {
                const remote = metadata.get(pair(id, field))
                if (remote) patchField(live, before, remote.value.conversation, metadataField)
            }
        } else if (kind === 'preset') {
            const live = database.botPresets.find((value) => value?.['id'] === id)
            const before = baseline.presets?.find((value) => value['id'] === id) ?? baseline.presetRecords.find((value) => value['id'] === id)
            if (!live || !before) continue
            const remote = presets.get(id)
            if (remote) patchField(live, before, remote.value, field)
        } else if (root && kind === 'exists' && id === 'persona') {
            const before = baseline.root.personas ?? [], live = database.personas ?? []
            const previous = before.find((value) => value.id === field), current = live.find((value) => value.id === field), remote = root.personas?.find((value) => value.id === field)
            const replace = (values: typeof live) => remote ? [...values.filter((value) => value.id !== field), canonicalClone(remote)] : values.filter((value) => value.id !== field)
            // A deleted ID is retired, so a local edit under it could never be saved.
            if (!remote || canonicalJson(previous ?? null) === canonicalJson(current ?? null)) {
                const selectedId = live[database.selectedPersona]?.id
                database.personas = replace(live)
                const index = database.personas.findIndex((value) => value.id === selectedId)
                database.selectedPersona = Math.max(0, index)
                // The fallback after a remote deletion is not a local selection to save.
                if (index < 0) (baseline.root as unknown as Record<string, unknown>).selectedPersona = database.personas[database.selectedPersona]?.id ?? database.selectedPersona
            }
            baseline.root.personas = replace(before)
        } else if (root && kind === 'persona') {
            const live = database.personas?.find((value) => value.id === id)
            const before = baseline.root.personas?.find((value) => value.id === id)
            const remote = root.personas?.find((value) => value.id === id)
            if (live && before && remote) patchField(live, before, remote, field)
        } else if (root && (kind === 'record' || kind === 'exists' && ['modules', 'loadouts', 'customModels'].includes(id))) {
            const identity = id === 'plugins' ? 'name' : 'id'
            const live = database as unknown as Record<string, unknown[]>, before = baseline.root as unknown as Record<string, unknown[]>, remote = root as unknown as Record<string, unknown[]>
            const itemId = (value: unknown) => (value as Record<string, unknown>)[identity]
            const original = before[id]?.find((value) => itemId(value) === field)
            const current = live[id]?.find((value) => itemId(value) === field)
            const next = remote[id]?.find((value) => itemId(value) === field)
            const replace = (values: unknown[] = []) => {
                const index = values.findIndex((value) => itemId(value) === field)
                if (next === undefined) { if (index >= 0) values.splice(index, 1) }
                else if (index >= 0) values[index] = canonicalClone(next)
                else values.push(canonicalClone(next))
                return values
            }
            // A deleted ID is retired, so a local edit under it could never be saved.
            if ((kind === 'exists' && next === undefined) || canonicalJson(current ?? null) === canonicalJson(original ?? null)) live[id] = replace(live[id])
            before[id] = replace(before[id])
        } else if (root && ['variable', 'toggle', 'preset-protected'].includes(kind)) {
            const rootField = kind === 'preset-protected' ? 'protectedPresetValues' : 'explicitGlobalChatVariables'
            const live = database as unknown as Record<string, Record<string, unknown>>, before = baseline.root as unknown as Record<string, Record<string, unknown>>, remote = root as unknown as Record<string, Record<string, unknown>>
            patchField(live[rootField] ??= {}, before[rootField] ??= {}, remote[rootField] ?? {}, id)
            live[rootField] = { ...live[rootField] }
        } else if (kind === 'order' && id === 'conversations') {
            const live = charactersById.get(field)
            const before = baselineById.get(field)
            if (!live || !before) continue
            const ids = orders.get(field) ?? []
            const order = new Map(ids.map((value,index) => [value,index]))
            const sort = (chats: Chat[]) => [...chats].sort((a,b) => (order.get(a.id) ?? Infinity) - (order.get(b.id) ?? Infinity))
            if (canonicalJson(live.chats.map((value) => value.id)) === canonicalJson(before.chats.map((value) => value.id))) live.chats = sort(live.chats)
            before.chats = sort(before.chats)
            const remoteDetail = details.get(field)
            if (remoteDetail) patchField(live, before, remoteDetail, 'chatFolders')
        } else if (root && kind === 'order' && ['modules', 'plugins', 'personas', 'loadouts', 'customModels'].includes(id)) {
            const identity = id === 'plugins' ? 'name' : 'id'
            const remote = root as unknown as Record<string, Record<string, unknown>[]>
            const order = new Map((remote[id] ?? []).map((item, index) => [item[identity], index]))
            const sort = (values: Record<string, unknown>[] = []) => [...values].sort((a,b) => (order.get(a[identity]) ?? Infinity) - (order.get(b[identity]) ?? Infinity))
            const live = database as unknown as Record<string, Record<string, unknown>[]>, before = baseline.root as unknown as Record<string, Record<string, unknown>[]>
            if (canonicalJson((live[id] ?? []).map((value) => value[identity])) === canonicalJson((before[id] ?? []).map((value) => value[identity]))) {
                const selectedId = id === 'personas' ? database.personas?.[database.selectedPersona]?.id : undefined
                live[id] = sort(live[id])
                if (id === 'personas') database.selectedPersona = Math.max(0, database.personas?.findIndex((value) => value.id === selectedId) ?? 0)
            }
            before[id] = sort(before[id])
        } else if (root && kind === 'order' && id === 'characters') {
            patchField(database, baseline.root, root, 'characterOrder')
        } else if ((kind === 'order' && id === 'presets') || (kind === 'exists' && id === 'preset')) {
            const selectedId = database.botPresets[database.botPresetsId]?.['id']
            const catalog = presetCatalog!
            const byId = new Map(database.botPresets.filter(Boolean).map((value) => [value['id'], value]))
            const next = catalog.items.map((summary) => byId.get(summary.id) ?? canonicalClone(presets.get(summary.id)?.value ?? { id: summary.id, name: summary.name, image: summary.image }) as botPreset)
            if (catalogPresets) {
                const loaded = catalog.items.filter((summary) => byId.has(summary.id) && isCatalogPresetLoaded(database.botPresets, summary.id))
                    .map((summary) => ({ summary, value: byId.get(summary.id)! }))
                const active = loaded.find(({ summary }) => summary.id === selectedId) ?? null
                database.botPresets = createCatalogPresetWorkingSet(catalog, active, loaded.filter((body) => body !== active))
                for (let index = 0; index < next.length; index++) database.botPresets[index] = next[index]
            } else database.botPresets = next
            const selectedIndex = database.botPresets.findIndex((value) => value['id'] === selectedId)
            database.botPresetsId = Math.max(0, selectedIndex)
            placeSelectedBody()
            // The fallback after a remote deletion is not a local selection to save.
            if (selectedIndex < 0) {
                const fallbackId = database.botPresets[database.botPresetsId]?.['id']
                const baselineRoot = baseline.root as unknown as Record<string, unknown>
                baselineRoot.botPresetsId = typeof fallbackId === 'string' ? fallbackId : database.botPresetsId
            }
            if (baseline.presets) baseline.presets = catalog.items.map((summary, index) => baseline.presets!.find((value) => value['id'] === summary.id) ?? canonicalClone(presets.get(summary.id)?.value ?? byId.get(summary.id) ?? next[index]))
        } else if (kind === 'exists' && id === 'character') {
            const summary = summaries.get(field)
            if (summary && !charactersById.has(field)) database.characters.push(createCatalogCharacterStub(summary))
            if (!summary) {
                database.characters = database.characters.filter((value) => value.chaId !== field)
                baseline.characters = baseline.characters.filter((value) => value.chaId !== field)
            }
        } else if (kind === 'exists' && id === 'conversation') {
            const live = charactersById.get(field)
            const before = baselineById.get(field)
            const conversationMetadata = metadata.get(pair(field, metadataField))
            if (live && before && conversationMetadata && !live.chats.some((value) => value.id === metadataField)) {
                const stub = createConversationSummaryStub(createConversationSummaryFromMetadata(field, conversationMetadata.value.conversation, live.chats.length, conversationMetadata.value.totalMessages, 0))
                live.chats.push(stub)
                before.chats.push({ ...conversationMetadata.value.conversation } as Chat)
            }
            if (live && before && !conversationMetadata) {
                live.chats = live.chats.filter((value) => value.id !== metadataField)
                before.chats = before.chats.filter((value) => value.id !== metadataField)
            }
        }
    }
    afterProjection?.()
    return baseline
}
