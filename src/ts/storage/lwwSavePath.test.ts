import { createPluginStorageStore } from '../plugins/pluginStorageStore'
import { setPluginStorageReloadHandler, PluginStorageObservation } from '../plugins/pluginStorageProtection'
import {deriveEffectivePresetMirrors, deriveEffectivePersonaMirrors, deriveEffectiveToggleVariables, flushEffectivePresetEdits, flushEffectivePersonaEdits, flushEffectiveToggleEdits, presetMirrorMap, getExplicitGlobalChatVariables, translateRootUnitIntents} from './effectiveIdentityState'
import { configurePersistentIdentityHooks } from './persistentIdentityHooks'
import { describe, expect, it, vi } from 'vitest'
import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { fixtureDatabase } from './tests/persistentDataFixtures'
import { PersistentMutationFencedError, SaveCoordinator } from './saveCoordinator'
import { capturePersistentRoot, createPersistentDataRuntime, type PersistentDataRuntimeStateAdapter } from './persistentDataRuntime'
import { applyLwwWorkingSetUnits, captureLwwWorkingSetBaseline } from './lwwWorkingSetApply'
import { createGeneratingConversationRegistry } from './generatingConversationRegistry'
import { diffRecordCollection } from './persistentUnitCapture'
import { createConversationSummaryStubFromChat } from './conversationResidency'
import { createCatalogCharacterStub, isCatalogCharacterStub } from './workingSetCatalog'
import { createMetadataOnlySelectedConversation, isMetadataOnlySelectedConversation } from './selectedConversationLifecycle'
import type { Database, Message, character } from './database.svelte'
import type { ConversationSummary, LwwStageReceive, PersistentDataStore, WorkingSetCommit } from './persistentDataStore'
import { createLeaseStub } from './persistentDataStore.testSupport'
import { planConversationInsertPages } from './conversationInsertPages'
import { PluginDeviceKeyspace } from '../plugins/pluginDeviceKeyspace'

async function harness() {
    const database = structuredClone(fixtureDatabase)
    database.modules = []
    database.personas = []
    database.botPresetsId = 0
    database.selectedPersona = 0
    const store = new IndexedDbPersistentDataStore(crypto.randomUUID(), new IDBFactory(), IDBKeyRange)
    await store.open()
    const imported = await store.replaceFromDatabase(database)
    const commit = vi.spyOn(store, 'commit')
    const coordinator = new SaveCoordinator({ store,
        captureRoot: () => capturePersistentRoot(database), capturePresets: () => database.botPresets,
        captureCharacters: () => database.characters, captureSelectedCharacter: () => database.characters[0],
        captureCharacter: (id) => database.characters.find((value) => value.chaId === id) ?? null,
        replaceDatabase: () => undefined })
    coordinator.initialize(imported.revision, { ...database, ...capturePersistentRoot(database) } as unknown as Database)
    return { database, store, commit, coordinator }
}

async function runtimeHarness(registry?: ReturnType<typeof createGeneratingConversationRegistry>, extraState?: Partial<PersistentDataRuntimeStateAdapter>, onLocalRevision?: (revision: number) => void) {
    const values = await harness()
    return initializeRuntimeHarness(values, registry, extraState, onLocalRevision)
}

async function initializeRuntimeHarness(values: Omit<Awaited<ReturnType<typeof harness>>, 'store'> & {store: PersistentDataStore}, registry?: ReturnType<typeof createGeneratingConversationRegistry>, extraState?: Partial<PersistentDataRuntimeStateAdapter>, onLocalRevision?: (revision: number) => void) {
    const {database,store} = values
    const runtime = createPersistentDataRuntime({store,state:{
        captureRoot:()=>capturePersistentRoot(database), capturePresets:()=>database.botPresets,
        captureCharacters:()=>database.characters, captureWorkingSetDatabase:()=>database,
        captureSelectedCharacter:()=>database.characters[0], captureCharacter:(id)=>database.characters.find((value)=>value.chaId===id)??null,
        getGeneratingConversations:()=>registry?.snapshot() ?? [],
        getSelectedCharacterId:()=>database.characters[0]?.chaId,replaceDatabase:()=>undefined,publishCharacter:()=>undefined,publishConversation:()=>undefined,
        ...extraState,
    },prepareDatabase:async(value)=>value,onLocalRevision})
    await runtime.initializeActiveWorkingSet(database)
    return {...values,store:store as PersistentDataStore,runtime}
}

async function identityRuntimeHarness() {
    const {database,store,commit} = await harness()
    Object.assign(database.botPresets[0],{mainPrompt:'First prompt',temperature:11})
    Object.assign(database.botPresets[1],{mainPrompt:'Second prompt',temperature:22})
    database.personas = [{id:'persona-first',name:'First persona',icon:'first.png',personaPrompt:'First persona prompt',note:'First note'},
        {id:'persona-second',name:'Second persona',icon:'second.png',personaPrompt:'Second persona prompt',note:'Second note'}]
    database.globalChatVariables = {toggle_mode:'explicit',other:'shared'}
    database.characters[0].chatPage = 0
    database.characters[0].chats[0].savedToggleValues = {toggle_mode:'initial bound'}
    const derive = () => {
        deriveEffectivePresetMirrors(database,(db,preset)=>{
            for (const [key,field] of Object.entries(presetMirrorMap)) {
                if (Object.hasOwn(preset,field)) (db as unknown as Record<string,unknown>)[key] = structuredClone(preset[field])
            }
        })
        deriveEffectivePersonaMirrors(database)
        deriveEffectiveToggleVariables(database,database.characters[0].chats[database.characters[0].chatPage])
    }
    derive()
    await store.replaceFromDatabase(database,(await store.readRoot()).revision)
    const runtime = createPersistentDataRuntime({store,state:{
        captureRoot:()=>capturePersistentRoot(database),capturePresets:()=>database.botPresets,captureCharacters:()=>database.characters,
        captureWorkingSetDatabase:()=>database,captureSelectedCharacter:()=>database.characters[0],captureCharacter:(id)=>database.characters.find((value)=>value.chaId===id)??null,
        getSelectedCharacterId:()=>database.characters[0].chaId,replaceDatabase:()=>undefined,publishCharacter:()=>undefined,publishConversation:()=>undefined,
        beforeCapture:()=>{flushEffectivePresetEdits(database);flushEffectivePersonaEdits(database);flushEffectiveToggleEdits(database)},
        afterRemoteApply:derive,
    },prepareDatabase:async(value)=>value})
    await runtime.initializeActiveWorkingSet(database)
    const native = store as PersistentDataStore
    const receive = async (mutations: WorkingSetCommit['unitMutations']) => {
        native.lwwStageReceive = async()=>undefined
        native.lwwApplyReceive = async()=>{
            const result = await store.commit({expectedRevision:runtime.revision,unitMutations:mutations})
            return {...result,affectedKeys:mutations!.map((value)=>value.key),heldKeys:[],deferredKeys:[]}
        }
        native.lwwFinishReceive = async()=>undefined
        // The staged rows are not read by these stubs; a real pull always stages at least one.
        const changes = mutations!.map((value)=>({key:value.key,stamp:{physicalMs:'1',logical:'0',writerId:'remote'},value:{kind:'deleted' as const}}))
        await runtime.applyLwwReceive({bindingAuthority:'identity',requestId:crypto.randomUUID(),changes,progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
    }
    return {database,store,commit,runtime,receive}
}

describe('LWW renderer save path', () => {
    it('invalidates a held device plugin cache after receiving plugin-local units without a plugin restart', async () => {
        let value = 'before'
        const keyspace = new PluginDeviceKeyspace('synthetic', {
            hydrate: async () => ({ complete: true, byteSize: value.length, entries: [{space:'string', key:'key', value}] }),
            read: async () => value, keys: async () => ['key'], write: async () => {},
        })
        expect(await keyspace.getItem('string', 'key')).toBe('before')
        const reload = vi.fn()
        const invalidated = vi.fn(() => keyspace.invalidate())
        const { runtime, store } = await runtimeHarness(undefined, { onPluginDeviceStorageChanged: invalidated, afterRemotePluginChange: reload })
        store.lwwStageReceive = async () => {}
        store.lwwApplyReceive = async () => {
            value = 'received'
            return { revision: runtime.revision, affectedKeys: ['["plugin-local","synthetic","string","key"]'], heldKeys: [], deferredKeys: [] }
        }
        store.lwwFinishReceive = async () => {}
        await runtime.applyLwwReceive({ bindingAuthority:'synthetic', requestId:'receive-local-plugin', changes:[], progress:{kind:'server',cursor:'1'}, admittedTimeUpperMs:'100' })
        expect(invalidated).toHaveBeenCalledWith('synthetic')
        expect(await keyspace.getItem('string', 'key')).toBe('received')
        expect(reload).not.toHaveBeenCalled()
    })

    it('stores every page of a newly added character with its final conversation order', async () => {
        const { store, coordinator } = await harness()
        const added = { type: 'character', chaId: 'paged-addition', name: 'Paged addition', chatPage: 1,
            chats: [
                { id: 'added-first', name: 'First', message: [{ role: 'user', data: 'first' }, { role: 'char', data: 'second' }] },
                { id: 'added-second', name: 'Second', message: [{ role: 'user', data: 'third' }] },
            ] } as unknown as character
        const plan = planConversationInsertPages({ expectedRevision: coordinator.revision, addCharacter: added }, 40)!
        let revision = coordinator.revision
        for (const step of plan.steps) revision = (await store.commit({ expectedRevision: revision, ...step })).revision
        expect((await store.readCharacter(added.chaId))!.value.chatPage).toBe(1)
        expect((await store.queryConversations({ characterId: added.chaId, order: 'configured', limit: 10 })).items.map((value) => value.id))
            .toEqual(added.chats.map((value) => value.id))
        for (const chat of added.chats) expect((await store.readConversation(added.chaId, chat.id!))!.value.message).toEqual(chat.message)
    })

    it('persists windowed recency and retains selected rows across an unrelated receive with later reads', async () => {
        const factory = new IDBFactory()
        const name = `recency-unrelated-receive-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(name, factory, IDBKeyRange) as PersistentDataStore
        let database = { username: 'Synthetic', botPresets: [], characters: [
            { type: 'character', chaId: 'selected', name: 'Selected', lastInteraction: 10, chatPage: 0,
                chats: [{ id: 'selected-chat', name: 'Chat', note: '', localLore: [], message: Array.from({ length: 12 }, (_, index) => ({ role: 'user', data: `synthetic-${index}`, chatId: `message-${index}` })) }] },
            { type: 'character', chaId: 'other', name: 'Other', lastInteraction: 20, chats: [] },
        ] } as unknown as Database
        await store.open()
        await store.replaceFromDatabase(database)
        const selected = () => database.characters.find((value) => value.chaId === 'selected')!
        const runtime = createPersistentDataRuntime({ store, state: {
            captureRoot: () => capturePersistentRoot(database), capturePresets: () => database.botPresets,
            captureCharacters: () => database.characters, captureWorkingSetDatabase: () => database,
            captureSelectedCharacter: selected, captureCharacter: (id) => database.characters.find((value) => value.chaId === id) ?? null,
            getSelectedCharacterId: () => 'selected', getSelectedConversationId: () => 'selected-chat',
            replaceDatabase: (value) => { database = value },
            publishCharacter: (value) => { database.characters[database.characters.findIndex((entry) => entry.chaId === value.chaId)] = value },
            publishConversation: (_id, conversation, next) => {
                if (next) database.characters[database.characters.findIndex((entry) => entry.chaId === next.chaId)] = next
                else selected().chats[selected().chats.findIndex((entry) => entry.id === conversation.id)] = conversation
            },
            canUseWindowedSelectedConversation: () => true, shouldHydrateFullCharacter: () => false, canReleaseConversation: () => true,
        }, prepareDatabase: async (value) => value })
        await runtime.initializeActiveWorkingSet(database)
        expect(await runtime.activateCharacter('selected')).toBe(true)
        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        const fullReads = vi.spyOn(store, 'readConversation')
        const windowReads = vi.spyOn(store, 'readConversationWindow')
        const beforeRecency = runtime.captureSelectedConversationAuthority()!
        expect(runtime.recordSelectedCharacterLastInteraction(beforeRecency, 10, 30)).toBe(true)
        await runtime.flushPendingDataLocally('send-recency')
        expect(fullReads).not.toHaveBeenCalled()
        expect(windowReads).not.toHaveBeenCalled()
        const reopened = new IndexedDbPersistentDataStore(name, factory, IDBKeyRange)
        await reopened.open()
        expect((await reopened.readCharacter('selected'))!.value.lastInteraction).toBe(30)
        expect((await reopened.queryCharacters({ order: 'recent', trash: false, limit: 10 })).items.map((value) => value.id)).toEqual(['selected', 'other'])
        const source = runtime.getActiveConversationViewportSource()!
        await source.ensureRange({ startIndex: 0, limit: 2, reason: 'viewport' })
        const pin = source.acquireRangePin(0, 2, 'editor')
        const snapshot = source.snapshot()
        const row = snapshot.rowAt(0)
        let mutations: NonNullable<WorkingSetCommit['unitMutations']> = [{ key: '["character","other","name"]', type: 'set', value: 'Received other' }]
        store.lwwStageReceive = async () => undefined
        store.lwwApplyReceive = async () => {
            const result = await store.commit({ expectedRevision: runtime.revision, unitMutations: mutations })
            return { ...result, affectedKeys: mutations.map((value) => value.key), heldKeys: [], deferredKeys: [] }
        }
        store.lwwFinishReceive = async () => undefined
        const receive = () => runtime.applyLwwReceive({ bindingAuthority: '1', requestId: crypto.randomUUID(), changes: [], progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' })
        await receive()
        expect(runtime.getActiveConversationViewportSource()).toBe(source)
        expect(source.snapshot().version).toBe(snapshot.version)
        expect(source.snapshot().rowAt(0)).toBe(row)
        expect(source.snapshot().keyAt(0)).toBe(snapshot.keyAt(0))
        expect(source.snapshot().storeRevision).toBe(runtime.revision)
        expect(runtime.captureSelectedConversationTarget()!.storeRevision).toBe(runtime.revision)
        await source.ensureRange({ startIndex: 8, limit: 2, reason: 'viewport' })
        expect(source.snapshot().rowAt(8)!.message.data).toBe('synthetic-8')
        const link = { id: 'received-preset', fingerprint: 'received fingerprint' }
        mutations = [
            { key: '["conversation","selected","selected-chat","note"]', type: 'set', value: 'Received note' },
            { key: '["conversation","selected","selected-chat","authorNotePreset"]', type: 'set', value: link },
        ]
        await receive()
        expect(runtime.getActiveConversationViewportSource()).not.toBe(source)
        expect(selected().chats[0].note).toBe('Received note')
        expect(selected().chats[0].authorNotePreset).toEqual(link)
        const afterReceiveRevision = runtime.revision
        await runtime.flushPendingDataLocally('after-relevant-receive')
        expect(runtime.revision).toBe(afterReceiveRevision)
        expect(selected().lastInteraction).toBe(30)
        pin.release()
    })

    it('derives received preset/persona selection and record changes then flushes without echo', async () => {
        const {database,store,commit,runtime,receive} = await identityRuntimeHarness()
        await receive([
            {key:'["root","botPresetsId"]',type:'set',value:'preset-alpha'},
            {key:'["root","selectedPersona"]',type:'set',value:'persona-second'},
            {key:'["preset","preset-alpha","mainPrompt"]',type:'set',value:'Received prompt'},
            {key:'["preset","preset-alpha","temperature"]',type:'set',value:42},
            {key:'["persona","persona-second","name"]',type:'set',value:'Received persona'},
        ])
        expect(database).toMatchObject({botPresetsId:1,selectedPersona:1,mainPrompt:'Received prompt',temperature:42,username:'Received persona',personaPrompt:'Second persona prompt'})
        expect(database.botPresets[0].mainPrompt).toBe('First prompt')
        commit.mockClear()
        await runtime.flushPendingDataLocally('after-received-identity')
        expect(commit).not.toHaveBeenCalled()
        expect((await store.readPreset('preset-alpha'))?.value.mainPrompt).toBe('Received prompt')
        await receive([{key:'["preset","preset-alpha","mainPrompt"]',type:'set',value:'Updated selected record'}])
        expect(database.mainPrompt).toBe('Updated selected record')
        commit.mockClear()
        await runtime.flushPendingDataLocally('after-received-selected-record')
        expect(commit).not.toHaveBeenCalled()
    })

    it.each([
        ['preset', '["root","botPresetsId"]', 'preset-alpha', '["exists","preset","preset-alpha"]'],
        ['persona', '["root","selectedPersona"]', 'persona-second', '["exists","persona","persona-second"]'],
        ['preset', '["root","botPresetsId"]', 'preset-alpha', '["root","botPresetsId"]'],
        ['persona', '["root","selectedPersona"]', 'persona-second', '["root","selectedPersona"]'],
    ] as const)('does not save the selection that replaces a remotely deleted selected %s (%s, %s, deleted %s)', async (kind, selectionKey, selectedId, existsKey) => {
        const {database,commit,runtime,receive} = await identityRuntimeHarness()
        await receive([{key:selectionKey,type:'set',value:selectedId}])
        await receive([{key:existsKey,type:'delete'}])
        const selected = kind === 'preset' ? database.botPresets[database.botPresetsId]['id'] : database.personas[database.selectedPersona].id
        expect(selected).toBe(kind === 'preset' ? 'preset-beta' : 'persona-first')
        commit.mockClear()
        await runtime.flushPendingDataLocally('after-remote-selected-deletion')
        expect(commit).not.toHaveBeenCalled()
    })

    it('does not report a received change as a local revision', async () => {
        const onLocalRevision = vi.fn()
        const {database,store,runtime} = await runtimeHarness(undefined, undefined, onLocalRevision)
        const before = runtime.revision
        const key = '["character","char-a","notes"]'
        store.lwwStageReceive = async()=>undefined
        store.lwwApplyReceive = async()=>{
            const result = await store.commit({expectedRevision:runtime.revision,unitMutations:[{type:'set',key,value:'remote'}]})
            return {...result,affectedKeys:[key],heldKeys:[],deferredKeys:[]}
        }
        store.lwwFinishReceive = async()=>undefined
        await runtime.applyLwwReceive({bindingAuthority:'a',requestId:'received-notes',changes:[{key,stamp:{physicalMs:'1',logical:'0',writerId:'remote'},value:{kind:'inline',bytes:'InJlbW90ZSI='}}],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
        expect(runtime.revision).toBe(before + 1)
        expect(onLocalRevision).not.toHaveBeenCalled()
        database.username = 'Local edit after a receive'
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingDataLocally('local-edit-after-receive')
        expect(onLocalRevision).toHaveBeenCalledExactlyOnceWith(before + 2)
    })

    it('captures receive baselines only for a pull that carries changes', async () => {
        const {store,runtime} = await runtimeHarness()
        const materialized = vi.spyOn(SaveCoordinator.prototype,'captureMaterializedBaseline')
        const presetRecords = vi.spyOn(SaveCoordinator.prototype,'capturePresetRecordBaseline')
        try {
            store.lwwStageReceive = async()=>undefined
            store.lwwApplyReceive = async()=>({revision:runtime.revision,affectedKeys:[],heldKeys:[],deferredKeys:[]})
            store.lwwFinishReceive = async()=>undefined
            const request = (changes: LwwStageReceive['changes']): LwwStageReceive => ({bindingAuthority:'a',requestId:crypto.randomUUID(),changes,progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
            await runtime.applyLwwReceive(request([]))
            expect(materialized).not.toHaveBeenCalled()
            expect(presetRecords).not.toHaveBeenCalled()
            await runtime.applyLwwReceive(request([{key:'["character","char-a","notes"]',stamp:{physicalMs:'1',logical:'0',writerId:'remote'},value:{kind:'inline',bytes:'InJlbW90ZSI='}}]))
            expect(materialized).toHaveBeenCalledOnce()
            expect(presetRecords).toHaveBeenCalledOnce()
        } finally {
            materialized.mockRestore()
            presetRecords.mockRestore()
        }
    })

    it('translates a plugin toggle intent after flushing an unsaved bound toggle edit', async () => {
        const {database,store,runtime} = await identityRuntimeHarness()
        configurePersistentIdentityHooks({beforeCapture:()=>flushEffectiveToggleEdits(database),afterRemoteApply:()=>undefined,
            translateRootUnitIntents:(mutations)=>translateRootUnitIntents(database,mutations)})
        try {
            const chat = database.characters[0].chats[0]
            database.globalChatVariables.toggle_mode = 'user edit'
            await runtime.commitPersistentUnitIntent('plugin-toggle',[{key:'["toggle","toggle_plugin"]',type:'set',value:'plugin'}])
            const stored = await store.readConversationMetadata(database.characters[0].chaId,chat.id!)
            expect(stored?.value.conversation.savedToggleValues).toEqual({toggle_mode:'user edit',toggle_plugin:'plugin'})
            expect(database.characters[0].chats[0].savedToggleValues).toEqual({toggle_mode:'user edit',toggle_plugin:'plugin'})
        } finally {
            configurePersistentIdentityHooks(null as never)
        }
    })

    it('derives received saved toggles without publishing explicit toggle changes on the next flush', async () => {
        const {database,store,commit,runtime,receive} = await identityRuntimeHarness()
        await receive([{key:'["conversation","char-b","conv-beta","savedToggleValues"]',type:'set',value:{toggle_mode:'received bound',toggle_new:'received new'}}])
        expect(database.globalChatVariables).toEqual({toggle_mode:'received bound',toggle_new:'received new',other:'shared'})
        expect(getExplicitGlobalChatVariables(database)).toEqual({toggle_mode:'explicit',other:'shared'})
        commit.mockClear()
        await runtime.flushPendingDataLocally('after-received-binding')
        expect(commit).not.toHaveBeenCalled()
        expect((await store.readRoot()).value.explicitGlobalChatVariables).toEqual({toggle_mode:'explicit',other:'shared'})
    })

    it('adopts accepted remote units without re-emitting them or resetting unrelated local edits', async () => {
        const {database,store,runtime,commit} = await runtimeHarness()
        store.lwwStageReceive = async()=>undefined
        store.lwwApplyReceive = async()=>{
            const revision=(await store.readRoot()).revision
            const result=await store.commit({expectedRevision:revision,unitMutations:[{key:'["character","char-a","notes"]',type:'set',value:'remote'}]})
            return {...result,affectedKeys:['["character","char-a","notes"]'],heldKeys:[],deferredKeys:[]}
        }
        store.lwwFinishReceive=async()=>undefined
        await runtime.applyLwwReceive({bindingAuthority:'a',requestId:'r',changes:[],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
        expect((database.characters[1] as character).notes).toBe('remote')
        commit.mockClear()
        await runtime.flushPendingDataLocally('after-remote')
        expect(commit).not.toHaveBeenCalled()
    })

    it('preserves stable preset and persona selections when remote catalogs reorder', async () => {
        const { database } = await harness()
        database.personas = [{ id: 'one', name: 'One' }, { id: 'two', name: 'Two' }] as Database['personas']
        database.selectedPersona = 1
        const selectedPreset = database.botPresets[0]['id']
        const baseline = captureLwwWorkingSetBaseline(database, capturePersistentRoot(database), database.botPresets)
        const root = { ...capturePersistentRoot(database), personas: [...database.personas].reverse() }
        const presets = [...database.botPresets].reverse()
        const reader = createLeaseStub({ readRoot: async () => ({ revision: 2, value: root }),
            queryPresets: async () => ({ revision: 2, items: presets.map((preset, configuredIndex) => ({ id: preset['id'], name: preset.name, configuredIndex })) }),
        })
        await applyLwwWorkingSetUnits(database, baseline, reader, ['["order","presets"]', '["order","personas"]'])
        expect(database.botPresets[database.botPresetsId]['id']).toBe(selectedPreset)
        expect(database.personas[database.selectedPersona].id).toBe('two')
        expect(baseline.root.selectedPersona).toBe('two')
    })

    it('finishes durable receive progress when renderer projection fails and requires refresh', async () => {
        const {store,runtime}=await runtimeHarness()
        store.lwwStageReceive=async()=>undefined
        store.lwwApplyReceive=async()=>({revision:runtime.revision,affectedKeys:['["character","char-a","notes"]'],heldKeys:[],deferredKeys:[]})
        store.lwwFinishReceive=vi.fn(async()=>undefined)
        vi.spyOn(store,'acquireRevision').mockRejectedValueOnce(new Error('projection unavailable'))
        await expect(runtime.applyLwwReceive({bindingAuthority:'a',requestId:'r',changes:[],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})).rejects.toThrow('projection unavailable')
        expect(store.lwwFinishReceive).toHaveBeenCalledOnce()
        expect(runtime.pendingWorkingSetRefreshRevision).toBe(runtime.revision)
    })

    it('adopts an explicit opaque local root edit without exposing an unknown received root unit', async () => {
        const {database,runtime,store} = await runtimeHarness()
        await runtime.commitPersistentUnitIntent('opaque-local',[{key:'["root","opaqueFutureField"]',type:'set',value:{local:true}}])
        expect((database as unknown as Record<string,unknown>).opaqueFutureField).toEqual({local:true})
        const baseline = captureLwwWorkingSetBaseline(database,capturePersistentRoot(database),database.botPresets)
        const reader = createLeaseStub({readRoot:async()=>({revision:runtime.revision,value:{...capturePersistentRoot(database),unknownReceived:'opaque',vertexAccessToken:'remote token',loreBookPage:99}})})
        await applyLwwWorkingSetUnits(database,baseline,reader,['["root","unknownReceived"]','["root","vertexAccessToken"]','["root","loreBookPage"]'])
        expect(database).not.toHaveProperty('unknownReceived')
        expect(database.vertexAccessToken).not.toBe('remote token')
        expect(database.loreBookPage).not.toBe(99)
        expect((await store.readRoot()).value).toHaveProperty('opaqueFutureField',{local:true})
    })

    it('applies received chat edit popup, color scheme, Realm creator block and loadout apply root settings', async () => {
        const {database} = await harness()
        database.risunestChatEditPopup = true
        const baseline = captureLwwWorkingSetBaseline(database,capturePersistentRoot(database),database.botPresets)
        const colorScheme = {...database.colorScheme,bgcolor:'#101010'}
        const blockedRealmCreators = [{id:'synthetic-creator',name:'Synthetic creator'}]
        const loadoutApplyOptions = {modules:false,globalVariables:true,preset:false,persona:true}
        const reader = createLeaseStub({readRoot:async()=>({revision:2,value:{...capturePersistentRoot(database),risunestChatEditPopup:false,colorScheme,blockedRealmCreators,loadoutApplyOptions}})})
        await applyLwwWorkingSetUnits(database,baseline,reader,['["root","risunestChatEditPopup"]','["root","colorScheme"]','["root","blockedRealmCreators"]','["root","loadoutApplyOptions"]'])
        expect(database.risunestChatEditPopup).toBe(false)
        expect(database.colorScheme).toEqual(colorScheme)
        expect(database.blockedRealmCreators).toEqual(blockedRealmCreators)
        expect(database.loadoutApplyOptions).toEqual(loadoutApplyOptions)
    })

    it('reports received root fields whose working-set value changed', async () => {
        const afterRemoteRootChange = vi.fn()
        const {database,store,runtime} = await runtimeHarness(undefined, {afterRemoteRootChange})
        const receive = async (key: string, value: unknown, duringReceive?: () => void) => {
            store.lwwStageReceive = async()=>{ duringReceive?.() }
            store.lwwApplyReceive = async()=>{
                const revision=(await store.readRoot()).revision
                const result=await store.commit({expectedRevision:revision,unitMutations:[{key:JSON.stringify(['root',key]),type:'set',value}]})
                return {...result,affectedKeys:[JSON.stringify(['root',key])],heldKeys:[],deferredKeys:[]}
            }
            store.lwwFinishReceive=async()=>undefined
            await runtime.applyLwwReceive({bindingAuthority:'a',requestId:crypto.randomUUID(),changes:[{key:JSON.stringify(['root',key]),stamp:{physicalMs:'1',logical:'0',writerId:'remote'},value:{kind:'deleted'}}],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
        }
        const colorSchemeName = database.colorSchemeName === 'light' ? 'dark' : 'light'

        await receive('colorSchemeName', colorSchemeName)
        expect(database.colorSchemeName).toBe(colorSchemeName)
        expect(afterRemoteRootChange).toHaveBeenCalledExactlyOnceWith(new Set(['colorSchemeName']))

        afterRemoteRootChange.mockClear()
        await receive('colorSchemeName', colorSchemeName)
        expect(afterRemoteRootChange).not.toHaveBeenCalled()

        await receive('colorSchemeName', 'remote-custom', () => { database.colorSchemeName = 'local-custom' })
        expect(database.colorSchemeName).toBe('local-custom')
        expect(afterRemoteRootChange).not.toHaveBeenCalled()
    })

    it('commits plugin units once if their pinned projection loses its revision', async () => {
        const {store,runtime,commit}=await runtimeHarness()
        vi.spyOn(store,'acquireRevision').mockRejectedValueOnce(new Error('pin lost'))
        await expect(runtime.commitPersistentUnitIntent('plugin',[{key:'["character","char-a","notes"]',type:'set',value:'durable'}])).rejects.toThrow('pin lost')
        expect(commit).toHaveBeenCalledOnce()
        expect((await store.readCharacter('char-a'))?.value['notes']).toBe('durable')
        expect(runtime.pendingWorkingSetRefreshRevision).toBe(runtime.revision)
    })

    it('captures fields and conversations of two characters independently of selection', async () => {
        const { database, store, commit, coordinator } = await harness()
        ;(database.characters[0] as character).notes = 'first field'
        ;(database.characters[1] as character).notes = 'second field'
        database.characters[0].chats[0].note = 'first metadata'
        database.characters[1].chats[0].message.push({ role: 'char', data: 'second reply', chatId: 'kept-message-id' })
        coordinator.markPersistentDataDirty(0)
        await coordinator.flushPendingDataLocally('two-characters')
        expect(((await store.readCharacter('char-b'))?.value as character).notes).toBe('first field')
        expect(((await store.readCharacter('char-a'))?.value as character).notes).toBe('second field')
        expect((await store.readConversation('char-b', 'conv-beta'))?.value.note).toBe('first metadata')
        expect((await store.readConversation('char-a', 'conv-long'))?.value.message.at(-1)?.chatId).toBe('kept-message-id')
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(commit.mock.calls[0][0].unitMutations?.map((value) => value.key)).toEqual(expect.arrayContaining([
            '["character","char-b","notes"]', '["character","char-a","notes"]', '["conversation","char-b","conv-beta","note"]',
        ]))
        commit.mockClear()
        coordinator.markPersistentDataDirty(0)
        await coordinator.flushPendingDataLocally('unchanged')
        expect(commit).not.toHaveBeenCalled()
    })

    it('persists changed opaque and device-local working-set fields and suppresses unchanged values', async () => {
        const {database,store,coordinator,commit} = await harness()
        const owner = database.characters[1] as character
        const chat = owner.chats[0]
        Object.assign(owner,{opaqueOwn:{local:'character'},chatPage:1,lastInteraction:123,statics:{messages:44}})
        Object.assign(chat,{opaqueOwn:{local:'chat'},isStreaming:true})
        Object.assign(database,{opaqueOwn:{local:'root'}})
        await coordinator.flushPendingDataLocally('opaque-local-observer')
        expect((await store.readCharacter(owner.chaId))?.value).toMatchObject({opaqueOwn:{local:'character'},chatPage:1,lastInteraction:123,statics:{messages:44}})
        expect((await store.readConversation(owner.chaId,chat.id!))?.value).toMatchObject({opaqueOwn:{local:'chat'},isStreaming:true})
        expect((await store.readRoot()).value).toHaveProperty('opaqueOwn',{local:'root'})
        commit.mockClear()
        await coordinator.flushPendingDataLocally('unchanged-local-opaque')
        expect(commit).not.toHaveBeenCalled()
    })

    it('projects shared remote fields while preserving opaque and local character/chat state', async () => {
        const {database} = await harness()
        const live = database.characters[1] as character
        Object.assign(live,{opaqueOwn:'local',lastInteraction:7,statics:{messages:4}})
        Object.assign(live.chats[0],{opaqueOwn:'local-chat',isStreaming:true})
        const baseline = captureLwwWorkingSetBaseline(database,capturePersistentRoot(database),database.botPresets)
        live['statics'].messages = 5
        const onboarding = {enabled:true,mode:'form' as const,questions:[{id:'q1',title:'Synthetic question',type:'text' as const,varName:'answer',options:[]}]}
        const reader = createLeaseStub({
            readCharacter:async()=>({revision:2,value:{...live,opaqueOwn:'remote',lastInteraction:99,statics:{messages:100,shared:42},onboarding}}),
            readConversationMetadata:async()=>({revision:2,value:{characterId:live.chaId,conversationId:live.chats[0].id,conversation:{...live.chats[0],opaqueOwn:'remote-chat',isStreaming:false,note:'shared note',onboardingDone:true},totalMessages:live.chats[0].message.length}}),
        })
        await applyLwwWorkingSetUnits(database,baseline,reader,['["character","char-a","opaqueOwn"]','["character","char-a","lastInteraction"]','["character","char-a","statics"]','["character","char-a","onboarding"]','["conversation","char-a","conv-long","opaqueOwn"]','["conversation","char-a","conv-long","isStreaming"]','["conversation","char-a","conv-long","note"]','["conversation","char-a","conv-long","onboardingDone"]'])
        expect(live).toMatchObject({opaqueOwn:'local',lastInteraction:7,statics:{messages:5,shared:42},onboarding})
        expect(live.chats[0]).toMatchObject({opaqueOwn:'local-chat',isStreaming:true,note:'shared note',onboardingDone:true})
    })

    it.each([false, true])('patches only catalog fields of received character units into a stub (kept baseline=%s)', async (keptBaseline) => {
        const {database} = await harness()
        const full = database.characters[2] as character
        delete (full as unknown as Record<string, unknown>).nickname
        const stub = createCatalogCharacterStub({id: full.chaId, name: full.name, image: full.image, configuredIndex: 2, recentAt: full.lastInteraction ?? 0,
            trashed: true, conversationCount: full.chats.length, type: full.type, trashTime: full.trashTime})
        if (!keptBaseline) database.characters[2] = stub
        const baseline = captureLwwWorkingSetBaseline(database, capturePersistentRoot(database), database.botPresets)
        database.characters[2] = stub
        const {trashTime: _trashTime, ...restored} = structuredClone(full)
        const reader = createLeaseStub({
            readCharacter: async () => ({revision: 2, value: {...restored, name: 'Restored', nickname: 'Remote nickname', chats: []}}),
        })
        await applyLwwWorkingSetUnits(database, baseline, reader, ['["character","char-c","trashTime"]', '["character","char-c","name"]', '["character","char-c","nickname"]'])
        expect(database.characters[2]).toBe(stub)
        expect(isCatalogCharacterStub(stub)).toBe(true)
        expect(stub).toMatchObject({name: 'Restored'})
        expect(stub).not.toHaveProperty('trashTime')
        expect(stub).not.toHaveProperty('nickname')
    })

    it('keeps received conversations and folders out of a stub that kept a baseline', async () => {
        const {database} = await harness()
        const full = database.characters[2] as character
        delete (full as unknown as Record<string, unknown>).chatFolders
        const baseline = captureLwwWorkingSetBaseline(database, capturePersistentRoot(database), database.botPresets)
        const stub = createCatalogCharacterStub({id: full.chaId, name: full.name, image: full.image, configuredIndex: 2, recentAt: full.lastInteraction ?? 0,
            trashed: true, conversationCount: full.chats.length, type: full.type, trashTime: full.trashTime})
        database.characters[2] = stub
        const folders = [{id: 'folder', name: 'Folder', folded: false}]
        const reader = createLeaseStub({
            readRoot: async () => ({revision: 2, value: capturePersistentRoot(database)}),
            readCharacter: async () => ({revision: 2, value: {...structuredClone(full), chatFolders: folders, chats: []}}),
            queryConversations: async () => ({revision: 2, items: [{id: 'conv-trash'}, {id: 'conv-new'}] as ConversationSummary[]}),
            readConversationMetadata: async () => ({revision: 2, value: {characterId: full.chaId, conversationId: 'conv-new', conversation: {id: 'conv-new', name: 'New', note: '', localLore: []}, totalMessages: 0}}),
        })
        await applyLwwWorkingSetUnits(database, baseline, reader, ['["exists","conversation","char-c","conv-new"]', '["order","conversations","char-c"]'])
        expect(database.characters[2]).toBe(stub)
        expect(isCatalogCharacterStub(stub)).toBe(true)
        expect(stub.chats).toEqual([])
        expect(stub).not.toHaveProperty('chatFolders')
    })

    it('patches received conversation metadata and messages in place without replacing chat objects', async () => {
        const {database} = await harness()
        const live = database.characters.find((value) => value.chaId === 'char-a')!
        const chat = live.chats.find((value) => value.id === 'conv-long')!
        const {message: _message, ...metadata} = chat
        const remoteMessages: Message[] = [...structuredClone(chat.message), {role: 'char', data: 'remote reply'}]
        const reader = createLeaseStub({
            readConversationMetadata: async () => ({revision: 2, value: {characterId: live.chaId, conversationId: chat.id, conversation: {...structuredClone(metadata), note: 'shared note'}, totalMessages: remoteMessages.length}}),
            readConversation: async () => ({revision: 2, value: {...structuredClone(metadata), message: remoteMessages}}),
        })
        const baseline = captureLwwWorkingSetBaseline(database, capturePersistentRoot(database), database.botPresets)
        await applyLwwWorkingSetUnits(database, baseline, reader, ['["conversation","char-a","conv-long","note"]', '["messages","char-a","conv-long"]'])
        expect(live.chats.find((value) => value.id === 'conv-long')).toBe(chat)
        expect(chat).toMatchObject({note: 'shared note'})
        expect(chat.message.at(-1)).toEqual({role: 'char', data: 'remote reply'})
    })

    it('keeps a metadata-only selected conversation shell when its received metadata is patched', async () => {
        const {database} = await harness()
        const live = database.characters.find((value) => value.chaId === 'char-a')!
        const index = live.chats.findIndex((value) => value.id === 'conv-long')
        const shell = createMetadataOnlySelectedConversation(live.chats[index])
        live.chats[index] = shell
        const {message, ...metadata} = structuredClone(fixtureDatabase.characters.find((value) => value.chaId === 'char-a')!.chats.find((value) => value.id === 'conv-long')!)
        const reader = createLeaseStub({
            readConversationMetadata: async () => ({revision: 2, value: {characterId: live.chaId, conversationId: metadata.id, conversation: {...metadata, note: 'shared note'}, totalMessages: message.length}}),
        })
        const baseline = captureLwwWorkingSetBaseline(database, capturePersistentRoot(database), database.botPresets)
        await applyLwwWorkingSetUnits(database, baseline, reader, ['["conversation","char-a","conv-long","note"]'])
        // Comparing by identity keeps the failure report from reading the shell's message accessor.
        expect(live.chats[index] === shell).toBe(true)
        expect(isMetadataOnlySelectedConversation(live.chats[index])).toBe(true)
        expect(shell.note).toBe('shared note')
    })

    it('retries changed units onto a newer revision without replacing unrelated values', async () => {
        const { database, store, coordinator } = await harness()
        await store.commit({ expectedRevision: coordinator.revision, unitMutations: [
            { key: '["character","char-a","desc"]', type: 'set', value: 'intervening description' },
        ] })
        ;(database.characters[1] as character).notes = 'intentional note'
        await coordinator.flushPendingDataLocally('revision-race')
        const current = (await store.readCharacter('char-a'))!.value as character
        expect(current.notes).toBe('intentional note')
        expect(current.desc).toBe('intervening description')
    })

    it('merges another writer\'s change to a chat whose messages a save replaces and saves both without a retry', async () => {
        const {database, store, runtime, commit} = await runtimeHarness()
        const acquireRevision = store.acquireRevision.bind(store)
        const readWorkingSetChangePage = vi.fn(async () => [{kind: 'conversation', key1: 'char-a', key2: 'conv-long'}])
        store.acquireRevision = async (revision) => Object.assign(await acquireRevision(revision), {readWorkingSetChangePage})
        const {revision} = await store.readRoot()
        await store.commit({expectedRevision: revision, unitMutations: [
            {key: '["conversation","char-a","conv-long","note"]', type: 'set', value: 'other writer note'},
        ]})
        const chat = database.characters.find((value) => value.chaId === 'char-a')!.chats.find((value) => value.id === 'conv-long')!
        chat.message.push({role: 'user', data: 'local reply'})
        commit.mockClear()

        await runtime.flushPendingDataLocally('concurrent-chat-change')

        const stored = (await store.readConversation('char-a', 'conv-long'))!.value
        expect(stored.message).toHaveLength(131)
        expect(stored.message.at(-1)).toMatchObject({role: 'user', data: 'local reply'})
        expect(stored.note).toBe('other writer note')
        expect(chat.note).toBe('other writer note')
        expect(readWorkingSetChangePage).toHaveBeenCalledWith(revision, null, expect.any(Number))
        expect(commit.mock.calls.map(([value]) => value.expectedRevision)).toEqual([revision, revision + 1])
        expect(commit.mock.calls[1][0].conversations).toEqual([expect.objectContaining({type: 'replace-range', characterId: 'char-a', conversationId: 'conv-long'})])
        expect(commit.mock.calls[1][0].unitMutations ?? []).not.toContainEqual(expect.objectContaining({key: '["conversation","char-a","conv-long","note"]'}))
    })

    it('does not replace unloaded conversation bodies with empty catalog placeholders', async () => {
        const { database, store, coordinator } = await harness()
        const original = database.characters[1].chats[0]
        database.characters[1].chats[0] = createConversationSummaryStubFromChat('char-a', original, 0)
        coordinator.initialize(coordinator.revision, database)
        ;(database.characters[1] as character).notes = 'detail only'
        await coordinator.flushPendingDataLocally('catalog-shell')
        expect((await store.readConversation('char-a', 'conv-long'))?.value.message).toHaveLength(130)
    })

    it('commits a created chat and its order without an unsplit size probe', async () => {
        const {database,store,commit,coordinator} = await harness()
        const owner = database.characters[0]
        const added = {...structuredClone(owner.chats[0]),id:'conv-added',name:'Added'}
        owner.chats.splice(1,0,added)
        const expected = owner.chats.map((chat) => chat.id)
        await coordinator.flushPendingDataLocally('large-added-chat')
        expect(commit).toHaveBeenCalledOnce()
        expect((await store.readConversation(owner.chaId,'conv-added'))?.value.message).toEqual(added.message)
        const stored = await store.queryConversations({characterId:owner.chaId,order:'configured',limit:128})
        expect(stored.items.map((item) => item.id)).toEqual(expected)
        commit.mockClear()
        await coordinator.flushPendingDataLocally('after-paged-chat')
        expect(commit).not.toHaveBeenCalled()
    })

    it('creates a conversation parent before committing its complete message list', async () => {
        const {store,coordinator} = await harness()
        await coordinator.commitPersistentUnitIntent('create-chat', [
            {key:'["exists","conversation","char-a","new-chat"]',type:'set',value:true},
            {key:'["conversation","char-a","new-chat","name"]',type:'set',value:'New'},
        ], [], [{characterId:'char-a',conversationId:'new-chat',messages:[{role:'char',data:'first',chatId:'untouched-message-id'}]}])
        expect((await store.readConversation('char-a','new-chat'))?.value).toMatchObject({name:'New',message:[{chatId:'untouched-message-id',data:'first'}]})
        await expect(coordinator.commitPersistentUnitIntent('missing-chat', [], [], [{characterId:'char-a',conversationId:'missing',messages:[]}])).rejects.toThrow('Missing conversation parent')
    })

    it('emits only one changed generic record and permits plugin reinstall', async () => {
        expect(diffRecordCollection('plugins', [{ name: 'a', source: 'old' }, { name: 'b', source: 'untouched' }],
            [{ name: 'a', source: 'new' }, { name: 'b', source: 'untouched' }])).toEqual([
            { key: '["record","plugins","a"]', type: 'set', value: { name: 'a', source: 'new' } },
        ])
        const { store, coordinator } = await harness()
        await coordinator.commitPersistentUnitIntent('install', [{ key: '["record","plugins","owner"]', type: 'set', value: { name: 'owner', source: 'first' } }])
        await coordinator.commitPersistentUnitIntent('remove', [{ key: '["record","plugins","owner"]', type: 'delete' }])
        await coordinator.commitPersistentUnitIntent('reinstall', [{ key: '["record","plugins","owner"]', type: 'set', value: { name: 'owner', source: 'second' } }])
        expect((await store.readRoot()).value.plugins).toEqual([{ name: 'owner', source: 'second' }])
    })

    it('pins export under paused writes while later saves wait', async () => {
        const { database, coordinator, commit } = await harness()
        let release!: () => void
        let entered!: () => void
        const waiting = new Promise<void>((resolve) => { release = resolve })
        const started = new Promise<void>((resolve) => { entered = resolve })
        const exportJob = coordinator.withPausedPersistentWrites('export', async (token) => { entered(); await waiting; return token.revision })
        await started
        database.username = 'later'
        const save = coordinator.flushPendingDataLocally('later-save')
        expect(commit).not.toHaveBeenCalled()
        release()
        await exportJob
        await save
        expect(commit).toHaveBeenCalledOnce()
    })

    it('drains completion immediately while another conversation keeps generating', async () => {
        const registry = createGeneratingConversationRegistry()
        const completeA = registry.register({characterId:'char-a',conversationId:'conv-long'})
        const completeB = registry.register({characterId:'char-b',conversationId:'conv-beta'})
        const {database,store,runtime,commit} = await runtimeHarness(registry)
        store.lwwStageReceive = async () => undefined
        store.lwwApplyReceive = async () => ({revision:runtime.revision,affectedKeys:[],heldKeys:[],deferredKeys:['["messages","char-a","conv-long"]','["messages","char-b","conv-beta"]']})
        store.lwwFinishReceive = async () => undefined
        store.lwwBindingState = async () => ({targetAuthority:'a'})
        store.lwwDrainDeferred = vi.fn(async (request) => {
            expect(request.generating).toEqual([{characterId:'char-b',conversationId:'conv-beta'}])
            const current = (await store.readConversation('char-a','conv-long'))!
            const result = await store.commit({expectedRevision:runtime.revision,conversations:[{type:'replace-range',characterId:'char-a',conversationId:'conv-long',start:current.value.message.length,deleteCount:0,messages:[{role:'char',data:'deferred remote',chatId:'remote-message-id'}]}]})
            return {...result,affectedKeys:['["messages","char-a","conv-long"]'],heldKeys:[],deferredKeys:['["messages","char-b","conv-beta"]']}
        })
        await runtime.applyLwwReceive({bindingAuthority:'a',requestId:'receive',changes:[],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
        completeA()
        await runtime.acknowledgeGenerationCompletion()
        expect(store.lwwDrainDeferred).toHaveBeenCalledOnce()
        expect(database.characters[1].chats[0].message.at(-1)?.chatId).toBe('remote-message-id')
        expect(database.characters[0].chats[0].message).toHaveLength(3)
        commit.mockClear()
        await runtime.flushPendingDataLocally('after-completion-drain')
        expect(commit).not.toHaveBeenCalled()
        completeB()
    })

    it.each(['staging', 'applying'] as const)('queues generation completion during receive %s without losing the response', async phase => {
        const registry = createGeneratingConversationRegistry()
        const complete = registry.register({ characterId: 'char-a', conversationId: 'conv-long' })
        const { database, store, runtime, commit } = await runtimeHarness(registry)
        const chat = database.characters.find(value => value.chaId === 'char-a')!.chats.find(value => value.id === 'conv-long')!
        const count = chat.message.length
        let entered!: () => void, release!: () => void
        const started = new Promise<void>(resolve => { entered = resolve })
        const waiting = new Promise<void>(resolve => { release = resolve })
        store.lwwStageReceive = async () => { if (phase === 'staging') { entered(); await waiting } }
        store.lwwFinishReceive = async () => {}
        store.lwwApplyReceive = vi.fn(async request => {
            expect(request.generating).toEqual(phase === 'applying' ? [{ characterId: 'char-a', conversationId: 'conv-long' }] : [])
            if (phase === 'applying') { entered(); await waiting }
            const result = await store.commit({ expectedRevision: runtime.revision,
                unitMutations: [{ key: '["character","char-b","desc"]', type: 'set', value: 'remote description' }],
                ...(phase === 'staging' ? { conversations: [{ type: 'replace-range' as const, characterId: 'char-a', conversationId: 'conv-long', start: 0, deleteCount: count, messages: [{ role: 'char', data: 'competing remote response', chatId: 'remote-response' }] }] } : {}),
            })
            return { ...result, affectedKeys: ['["character","char-b","desc"]', ...(phase === 'staging' ? ['["messages","char-a","conv-long"]'] : [])], heldKeys: [], deferredKeys: phase === 'applying' ? ['["messages","char-a","conv-long"]'] : [] }
        })
        const receive = runtime.applyLwwReceive({ bindingAuthority: 'a', requestId: 'foreground', changes: [], progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' })
        await started
        chat.message.push({ role: 'char', data: 'completed while receiving', chatId: 'local-completion' })
        const calls = commit.mock.calls.length
        complete()
        const save = runtime.acknowledgeGenerationCompletion()
        await Promise.resolve()
        expect(commit.mock.calls).toHaveLength(calls)
        release(); await receive; await save
        const stored = (await store.readConversation('char-a', 'conv-long'))!.value
        expect(stored.message).toHaveLength(count + 1)
        expect(stored.message.at(-1)).toMatchObject({ data: 'completed while receiving', chatId: 'local-completion' })
        expect((database.characters.find(value => value.chaId === 'char-b') as character).desc).toBe('remote description')
    })

    it('drains a completed durable receive on runtime restart without another remote receive', async () => {
        const registry = createGeneratingConversationRegistry()
        registry.register({characterId:'char-a',conversationId:'conv-long'})
        const first = await runtimeHarness(registry)
        const {store, commit, database} = first
        let deferred = false
        store.lwwBindingState = async () => ({targetAuthority:'7'})
        const stage = vi.fn(async () => undefined)
        store.lwwStageReceive = stage
        store.lwwFinishReceive = async () => undefined
        store.lwwApplyReceive = async () => {
            deferred = true
            return {revision:first.runtime.revision,affectedKeys:[],heldKeys:[],deferredKeys:['["messages","char-a","conv-long"]']}
        }
        store.lwwDrainDeferred = vi.fn(async (request) => {
            expect(request.bindingAuthority).toBe('7')
            expect(request.generating).toEqual([])
            const current = (await store.readConversation('char-a','conv-long'))!
            const result = await store.commit({expectedRevision:current.revision,conversations:[{type:'replace-range',characterId:'char-a',conversationId:'conv-long',start:current.value.message.length,deleteCount:0,messages:[{role:'char',data:'restart remote',chatId:'restart-remote-id'}]}]})
            deferred = false
            return {...result,affectedKeys:['["messages","char-a","conv-long"]'],heldKeys:[],deferredKeys:[]}
        })
        await first.runtime.applyLwwReceive({bindingAuthority:'7',requestId:'completed-before-crash',changes:[],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
        expect(deferred).toBe(true)
        stage.mockClear()
        const restarted = await initializeRuntimeHarness(first)
        expect(deferred).toBe(false)
        expect(store.lwwDrainDeferred).toHaveBeenCalledOnce()
        expect(store.lwwStageReceive).not.toHaveBeenCalled()
        expect(database.characters.find(value=>value.chaId==='char-a')!.chats[0].message.at(-1)?.chatId).toBe('restart-remote-id')
        commit.mockClear()
        await restarted.runtime.flushPendingDataLocally('restart-adopted-baseline')
        expect(commit).not.toHaveBeenCalled()
    })

    it('keeps native drain optional for web startup and generation completion', async () => {
        const values = await harness()
        const binding = vi.fn(async () => ({targetAuthority:'9'}))
        const store = values.store as PersistentDataStore
        store.lwwBindingState = binding
        const {runtime} = await initializeRuntimeHarness(values)
        await runtime.acknowledgeGenerationCompletion()
        expect(binding).not.toHaveBeenCalled()
    })

    it('uses the current binding for completion after switching without a new receive', async () => {
        const {store,runtime} = await runtimeHarness()
        let authority = '11'
        store.lwwBindingState = async () => ({targetAuthority:authority})
        store.lwwStageReceive = async () => undefined
        store.lwwApplyReceive = async () => ({revision:runtime.revision,affectedKeys:[],heldKeys:[],deferredKeys:[]})
        store.lwwFinishReceive = async () => undefined
        store.lwwDrainDeferred = vi.fn(async request => {
            if (request.bindingAuthority !== authority) throw new Error('binding-authority-changed')
            return {revision:runtime.revision,affectedKeys:[],heldKeys:[],deferredKeys:[]}
        })
        await runtime.applyLwwReceive({bindingAuthority:'11',requestId:'old-binding-receive',changes:[],progress:{kind:'server',cursor:'1'},admittedTimeUpperMs:'100'})
        authority = '12'
        await expect(runtime.acknowledgeGenerationCompletion()).resolves.toBeUndefined()
        expect(store.lwwDrainDeferred).toHaveBeenCalledWith(expect.objectContaining({bindingAuthority:'12',generating:[]}))
    })

    it('drains held receives after a response-free generation without acknowledging completion', async () => {
        const registry = createGeneratingConversationRegistry()
        const release = registry.register({characterId:'char-a',conversationId:'conv-long'})
        const {store,runtime,database,commit} = await runtimeHarness(registry)
        store.lwwBindingState = async () => ({targetAuthority:'current-target'})
        store.lwwDrainDeferred = vi.fn(async (request) => {
            expect(request.generating).toEqual([])
            const result = await store.commit({expectedRevision:runtime.revision,unitMutations:[{
                key:'["character","char-a","desc"]',type:'set',value:'Held remote edit',
            }]})
            return {...result,affectedKeys:['["character","char-a","desc"]'],heldKeys:[],deferredKeys:[]}
        })
        const acknowledge = vi.spyOn(runtime, 'acknowledgeGenerationCompletion')
        const epoch = runtime.getStorageAuthorityEpoch()
        release()
        await runtime.drainLwwDeferred(epoch)
        expect(acknowledge).not.toHaveBeenCalled()
        expect(database.characters.find((value) => value.chaId === 'char-a')).toMatchObject({desc:'Held remote edit'})
        expect(store.lwwDrainDeferred).toHaveBeenCalledWith(expect.objectContaining({bindingAuthority:'current-target'}))
        const calls = commit.mock.calls.length
        await runtime.flushPendingDataLocally('response-free-drain-no-echo')
        expect(commit.mock.calls).toHaveLength(calls)
    })

    it('rejects a response-free deferred drain admitted under a stale authority epoch', async () => {
        const {store,runtime} = await runtimeHarness()
        store.lwwBindingState = vi.fn(async () => ({targetAuthority:'current-target'}))
        store.lwwDrainDeferred = vi.fn(async () => ({revision:runtime.revision,affectedKeys:[],heldKeys:[],deferredKeys:[]}))
        await expect(runtime.drainLwwDeferred(runtime.getStorageAuthorityEpoch() - 1)).rejects.toThrow(PersistentMutationFencedError)
        expect(store.lwwBindingState).not.toHaveBeenCalled()
        expect(store.lwwDrainDeferred).not.toHaveBeenCalled()
    })

    it('registers all generating conversations using both stable IDs', () => {
        const registry = createGeneratingConversationRegistry()
        const releaseA = registry.register({ characterId: 'a', conversationId: 'same' })
        const releaseB = registry.register({ characterId: 'b', conversationId: 'same' })
        const secondA = registry.register({ characterId: 'a', conversationId: 'same' })
        releaseA()
        expect(registry.snapshot()).toHaveLength(2)
        secondA()
        expect(registry.snapshot()).toEqual([{ characterId: 'b', conversationId: 'same' }])
        releaseB()
        expect(registry.snapshot()).toEqual([])
    })

    it('flushes capture before native apply and refreshes unselected fields without initializing again', async () => {
        const { database, store, coordinator } = await harness()
        const registry = createGeneratingConversationRegistry()
        registry.register({ characterId: 'char-a', conversationId: 'conv-long' })
        const stage = vi.fn(async () => {
            expect(((await store.readCharacter('char-b'))?.value as character).notes).toBe('uncaptured local')
        })
        const apply = vi.fn(async () => {
            const revision = (await store.readRoot()).revision
            const result = await store.commit({ expectedRevision: revision, unitMutations: [{ key: '["character","char-a","desc"]', type: 'set', value: 'remote field' }] })
            return { revision: result.revision, affectedKeys: ['["character","char-a","desc"]'], heldKeys: [], deferredKeys: ['["messages","char-a","conv-long"]'] }
        })
        const finish = vi.fn(async () => undefined)
        const native = store as PersistentDataStore
        native.lwwStageReceive = stage
        native.lwwApplyReceive = apply
        native.lwwFinishReceive = finish
        const runtime = createPersistentDataRuntime({ store: native, state: {
            captureRoot: () => capturePersistentRoot(database), capturePresets: () => database.botPresets,
            captureCharacters: () => database.characters, captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((value) => value.chaId === id) ?? null,
            captureWorkingSetDatabase: () => database, getSelectedCharacterId: () => 'char-b',
            getGeneratingConversations: () => registry.snapshot(), replaceDatabase: () => undefined,
            publishCharacter: () => undefined, publishConversation: () => undefined,
        }, prepareDatabase: async (value) => value })
        await runtime.initializeActiveWorkingSet(database)
        const initialize = vi.spyOn(SaveCoordinator.prototype, 'initialize')
        ;(database.characters[0] as character).notes = 'uncaptured local'
        await runtime.applyLwwReceive({ bindingAuthority: '1', requestId: 'receive', changes: [], progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' } as LwwStageReceive)
        expect(apply).toHaveBeenCalledWith({ bindingAuthority: '1', requestId: 'receive', generating: [{ characterId: 'char-a', conversationId: 'conv-long' }] })
        expect((database.characters[1] as character).desc).toBe('remote field')
        expect((database.characters[0] as character).notes).toBe('uncaptured local')
        expect(finish).toHaveBeenCalledOnce()
        expect(initialize).not.toHaveBeenCalled()
        initialize.mockRestore()
    })
})


describe('received plugin storage protection', () => {
    async function pluginHarness() {
        const reload = vi.fn()
        setPluginStorageReloadHandler(reload)
        let storage!: ReturnType<typeof createPluginStorageStore>
        const h = await runtimeHarness(undefined, { onPluginStorageChanged: owner => storage?.invalidateOwner(owner) })
        const owner = crypto.randomUUID()
        storage = createPluginStorageStore({ store: h.store,
            getStorageAuthorityEpoch: () => h.runtime.getStorageAuthorityEpoch(),
            assertPersistentMutationAllowed: epoch => h.runtime.assertPersistentMutationAllowed(epoch),
            mutate: (mutations, check) => h.runtime.mutatePersistentPluginStorage('synthetic-plugin', mutations, check),
        })
        const guest = storage.forOwner(owner)
        await guest.setItem('settings', { selected: 'old', refreshed: 0 })
        const nativeCommit = h.store.commit.bind(h.store)
        const localChanges: WorkingSetCommit[] = []
        h.store.commit = async change => { localChanges.push(change); return nativeCommit(change) }
        h.store.lwwStageReceive = async () => {}
        h.store.lwwFinishReceive = async () => {}
        const receive = async (value: unknown, options: { pause?: Promise<void>; unknown?: boolean; projectionFailure?: boolean; echo?: boolean } = {}) => {
            const key = JSON.stringify(['plugin', owner, 'settings'])
            h.store.lwwApplyReceive = async () => {
                await options.pause
                if (options.echo) return { revision: h.runtime.revision, affectedKeys: [], heldKeys: [], deferredKeys: [] }
                const result = await nativeCommit({ expectedRevision: h.runtime.revision, pluginStorage: [value === undefined
                    ? { type: 'delete', owner, key: 'settings' } : { type: 'set', owner, key: 'settings', value }] })
                if (options.unknown) throw new Error('unknown durable outcome')
                if (options.projectionFailure) vi.spyOn(h.store, 'acquireRevision').mockRejectedValueOnce(new Error('projection unavailable'))
                return { ...result, affectedKeys: [key], heldKeys: [], deferredKeys: [] }
            }
            return h.runtime.applyLwwReceive({ bindingAuthority: 'synthetic', requestId: crypto.randomUUID(),
                changes: [{ key, stamp: { physicalMs: '1', logical: '0', writerId: 'client-a' }, value: { kind: 'deleted' } }],
                progress: { kind: 'server', cursor: '1' }, admittedTimeUpperMs: '100' })
        }
        return { ...h, owner, guest, storage, receive, localChanges, reload }
    }

    it.each(['startup', 'timer', 'unload'])('keeps client A durable while client B republishes its whole cached value at %s', async activity => {
        const h = await pluginHarness()
        const cached = await h.guest.getItem('settings') as { selected: string; refreshed: number }
        await h.receive({ selected: 'client-a', refreshed: 1 })
        h.localChanges.length = 0
        await expect(h.guest.setItem('settings', { ...cached, refreshed: activity.length })).rejects.toThrow('PLUGIN_STORAGE_STALE')
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toEqual({ selected: 'client-a', refreshed: 1 })
        expect(h.localChanges).toEqual([])
        expect(h.reload).toHaveBeenCalled()
        const reloaded = h.storage.forOwner(h.owner)
        const fresh = await reloaded.getItem('settings') as object
        await reloaded.setItem('settings', { ...fresh, refreshed: 2 })
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toEqual({ selected: 'client-a', refreshed: 2 })
        h.guest.dispose(); reloaded.dispose()
    })

    it('checks a write queued behind receive, preserving writes ordered before it', async () => {
        const h = await pluginHarness()
        await h.guest.getItem('settings')
        await h.guest.setItem('settings', { selected: 'before-receive' })
        let release!: () => void
        const pause = new Promise<void>(resolve => { release = resolve })
        const receiving = h.receive({ selected: 'client-a' }, { pause })
        const writing = h.guest.setItem('settings', { selected: 'stale-timer' })
        const rejected = expect(writing).rejects.toThrow('PLUGIN_STORAGE_STALE')
        release(); await receiving; await rejected
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toEqual({ selected: 'client-a' })
        h.guest.dispose()
    })

    it('rejects clear and deletion without recreating remote deletions, but permits blind writes and unaffected keys', async () => {
        const h = await pluginHarness()
        await h.receive(undefined)
        await expect(h.guest.clear()).rejects.toThrow('PLUGIN_STORAGE_STALE')
        await expect(h.guest.removeItem('settings')).rejects.toThrow('PLUGIN_STORAGE_STALE')
        expect(await h.store.readPluginStorage(h.owner, 'settings')).toBeNull()
        await h.guest.setItem('unaffected', 'valid cleanup')
        const blind = h.storage.forOwner(h.owner)
        await blind.keys()
        await blind.setItem('settings', 'blind')
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toBe('blind')
        h.guest.dispose(); blind.dispose()
    })

    it.each([{ unknown: true }, { projectionFailure: true }])('fences a committed value before failed projection or an unknown apply result: %j', async options => {
        const h = await pluginHarness()
        await expect(h.receive('remote', options)).rejects.toThrow()
        await expect(h.guest.setItem('settings', 'old')).rejects.toThrow()
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toBe('remote')
        h.guest.dispose()
    })

    it('does not advance for an echo and documents the pre-receive LWW boundary', async () => {
        const h = await pluginHarness()
        await h.guest.getItem('settings')
        await h.receive(null, { echo: true })
        await h.guest.setItem('settings', 'committed before learning of remote update')
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toBe('committed before learning of remote update')
        expect(h.reload).not.toHaveBeenCalled()
        h.guest.dispose()
    })

    it('observes the keys a blind clear actually deleted', async () => {
        const h = await pluginHarness()
        const blind = h.storage.forOwner(h.owner)
        await blind.clear()
        h.guest.dispose()
        await h.receive('remote reintroduction')
        expect(h.reload).toHaveBeenCalled()
        await expect(blind.setItem('settings', 'stale')).rejects.toThrow('PLUGIN_STORAGE_STALE')
        blind.dispose()
    })

    it('a fresh empty owner snapshot observes remote deletions', async () => {
        const h = await pluginHarness()
        await h.receive(undefined)
        await expect(h.guest.clear()).rejects.toThrow('PLUGIN_STORAGE_STALE')
        await expect(h.guest.snapshot()).resolves.toEqual({})
        await expect(h.guest.clear()).resolves.toBeUndefined()
        h.guest.dispose()
    })

    it('fences observations after an unknown deferred drain outcome', async () => {
        const h = await pluginHarness()
        h.store.lwwBindingState = async () => ({ targetAuthority: 'synthetic' })
        h.store.lwwDrainDeferred = async () => { throw new Error('unknown drain') }
        await expect(h.runtime.drainLwwDeferred()).rejects.toThrow('unknown drain')
        await expect(h.guest.setItem('settings', 'stale')).rejects.toThrow('PLUGIN_STORAGE_STALE')
        h.guest.dispose()
    })

    it('checks database-permission unit commits inside the same receive queue', async () => {
        const h = await pluginHarness()
        const observation = new PluginStorageObservation(h.owner)
        observation.observe('settings')
        const mutation = { type: 'set' as const, owner: h.owner, key: 'settings', value: 'stale database snapshot' }
        const check = observation.prepareWrite([mutation])
        await h.receive('remote')
        h.localChanges.length = 0
        await expect(h.runtime.commitPersistentUnitIntent('database-permission', [{ key: JSON.stringify(['plugin', h.owner, 'settings']), type: 'set', value: mutation.value }], [], [], undefined, check)).rejects.toThrow('PLUGIN_STORAGE_STALE')
        expect((await h.store.readPluginStorage(h.owner, 'settings'))?.value).toBe('remote')
        expect(h.localChanges).toEqual([])
        observation.dispose(); h.guest.dispose()
    })
})
