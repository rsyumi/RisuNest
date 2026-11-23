import {beforeEach,describe,expect,it,onTestFinished,vi} from 'vitest'
vi.mock('../platform',async()=>(await import('../platform.testSupport')).platformModule({runtime:'tauri',os:'windows'}))
const plugins=vi.hoisted(()=>({fencePluginExecutionForAuthorityReplacement:vi.fn(async()=>{}),invalidatePluginCachesAfterAuthorityReplacement:vi.fn(async()=>{}),restartPluginsAfterAuthorityReplacement:vi.fn(async()=>{})}))
const binding=vi.hoisted(()=>({bound:true,state:{targetAuthority:'9007199254740993'},fence:vi.fn(async()=>{}),assertAuthority:vi.fn(async()=>{}),resume:vi.fn(async()=>{}),release:vi.fn(()=>{})}))
const bindingBoundary=vi.hoisted(()=>({prepare:vi.fn(),invoke:vi.fn()}))
vi.mock('./sync/bindingRegistry',async original=>({...await original<typeof import('./sync/bindingRegistry')>(),prepareBoundLibraryReplacement:bindingBoundary.prepare}))
vi.mock('@tauri-apps/api/core',async original=>({...await original<typeof import('@tauri-apps/api/core')>(),invoke:bindingBoundary.invoke}))
const alerts=vi.hoisted(()=>({checkbox:vi.fn(async()=>({confirmed:true,checked:true}))}))
vi.mock('../alert',()=>({alertCheckboxConfirm:alerts.checkbox}))
vi.mock('../plugins/apiV3/v3.svelte',()=>plugins)
const library=vi.hoisted(()=>({hasCharacters:vi.fn<()=>Promise<boolean>>()}))
vi.mock('./libraryCharacters',()=>({hasLibraryCharacters:library.hasCharacters}) satisfies typeof import('./libraryCharacters'))
import type {Database} from './database.svelte'
import type {PersistentRevisionLease} from './persistentDataStore'
import {createLeaseStub,createStoreStub} from './persistentDataStore.testSupport'
import {capturePersistentPluginStorage,capturePersistentPresets,capturePersistentRoot,createPersistentDataRuntime} from './persistentDataRuntime'
import type {OfficialRevisionPublisher} from './saveCoordinator'
import {makeDatabase} from './saveCoordinator.testSupport'
import {runNativeBlockRisuSaveRestore,type NativeFileJobStatus} from './nativeFileJobs'
import {retryCommittedWorkingSetRefreshWithContinuation} from './committedWorkingSetContinuation'
import {restoreNativeOfficialAccountCandidate} from './sync/nativeOfficialAccountFlow'
import {createPluginStartupGate} from '../plugins/pluginStartupGate'
import {subscribeSyncBindingChanges} from './sync/bindingChanges'
import {get} from 'svelte/store'
async function createHarness(officialPublisher?: OfficialRevisionPublisher, captureWorkingSet = false) {
    let database = makeDatabase()
    let durable = structuredClone(database)
    let revision = 1
    let generating = false
    const leases: PersistentRevisionLease[] = []
    const replaceDatabase = vi.fn((replacement: Database) => { database = replacement })
    const onWorkingSetRefreshRequired = vi.fn()
    const onBackgroundError = vi.fn()
    const publishPresetWorkingSet = vi.fn()
    const store = createStoreStub({
        commitWorkingSetChangeCursor:vi.fn(async()=>{}),
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision, value: capturePersistentRoot(durable) })),
        commit: vi.fn(async () => { throw new Error('Unexpected content commit') }),
        replaceFromDatabase: vi.fn(async (replacement: Database, expected: number) => {
            expect(expected).toBe(revision)
            durable = structuredClone(replacement)
            return { revision: ++revision }
        }),
        acquireRevision: vi.fn(async (expected: number) => {
            expect(expected).toBe(revision)
            const snapshot = structuredClone(durable)
            const lease = createLeaseStub({
                revision: expected,
                readRoot: vi.fn(async () => ({ revision: expected, value: capturePersistentRoot(snapshot) })),
                queryPresets: vi.fn(async () => ({ revision: expected, items: [] })),
                readPreset: vi.fn(async () => null),
                queryCharacters: vi.fn(async ({ trash }: { trash: boolean }) => ({
                    revision: expected,
                    items: trash ? [] : snapshot.characters.map((character, configuredIndex) => ({
                        id: character.chaId,
                        name: character.name,
                        type: character.type,
                        configuredIndex,
                        recentAt: 0,
                        trashed: false,
                        conversationCount: 0,
                    })),
                })),
                readCharacterSummary: vi.fn(async (id: string) => {
                    const configuredIndex = snapshot.characters.findIndex((character) => character.chaId === id)
                    if (configuredIndex < 0) return null
                    const character = snapshot.characters[configuredIndex]
                    return {
                        id, name: character.name, type: character.type, configuredIndex,
                        recentAt: 0, trashed: false, conversationCount: 0,
                    }
                }),
                readCharacter: vi.fn(async (id: string) => {
                    const value = snapshot.characters.find((character) => character.chaId === id)
                    if (!value) return null
                    const { chats: _chats, ...detail } = value
                    return { revision: expected, value: detail }
                }),
                queryConversations: vi.fn(async () => ({ revision: expected, items: [] })),
                queryPluginStorage: vi.fn(async () => ({ revision: expected, items: [] })),
                release: vi.fn(async () => undefined),
            })
            leases.push(lease)
            return lease
        }),
        materializeDatabase: vi.fn(async () => { throw new Error('Unexpected complete materialization') }),
    })
    const runtime = createPersistentDataRuntime({
        store,
        state: {
            captureWorkingSetDatabase: captureWorkingSet ? () => database : undefined,
            captureRoot: () => capturePersistentRoot(database),
            capturePluginStorage: () => capturePersistentPluginStorage(database),
            capturePresets: () => capturePersistentPresets(database),
            captureSelectedCharacter: () => database.characters[0] ?? null,
            captureCharacter: (id) => database.characters.find((value) => value.chaId === id) ?? null,
            getSelectedCharacterId: () => database.characters[0]?.chaId,
            getSelectedConversationId: () => null,
            replaceDatabase,
            publishCharacter: vi.fn(),
            publishConversation: vi.fn(),
            publishPresetWorkingSet,
            isConversationOperationActive: () => generating,
        },
        prepareDatabase: async (value) => structuredClone(value),
        officialPublisher,
        onWorkingSetRefreshRequired,
        onBackgroundError,
        clock: { setTimeout: vi.fn(() => 1), clearTimeout: vi.fn() },
    })
    await runtime.initializeActiveWorkingSet(database)
    return {
        runtime, store, replaceDatabase, publishPresetWorkingSet,
        onWorkingSetRefreshRequired, onBackgroundError, leases,
        get database() { return database },
        get durable() { return durable },
        set generating(value: boolean) { generating = value },
        nativeCommit(replacement: Database) {
            durable = structuredClone(replacement)
            return ++revision
        },
    }
}

beforeEach(()=>{vi.clearAllMocks();bindingBoundary.prepare.mockReset().mockResolvedValue(binding);bindingBoundary.invoke.mockReset();library.hasCharacters.mockReset().mockResolvedValue(true);plugins.restartPluginsAfterAuthorityReplacement.mockImplementation(async()=>{});binding.bound=true;binding.fence.mockClear();binding.assertAuthority.mockClear();binding.resume.mockReset();binding.resume.mockImplementation(async()=>{})})
describe('direct native upstream import publication',()=>{
    it('releases an abandoned preparation hold even when authority lookup fails, allowing one-time plugin recovery',async()=>{
        const registry=await vi.importActual<typeof import('./sync/bindingRegistry')>('./sync/bindingRegistry')
        const target={kind:'server' as const,connectionId:'synthetic-failed-preparation'}
        const state={target,targetAuthority:'1',selectionEpoch:'1',libraryId:'synthetic',progress:null}
        let unavailable=false,starts=0
        bindingBoundary.invoke.mockImplementation(async command=>{if(unavailable)throw new Error('native state unavailable');if(command==='pds_lww_binding_state')return state;throw new Error(command)})
        bindingBoundary.prepare.mockImplementation(registry.prepareBoundLibraryReplacement)
        const unsupported=async():Promise<never>=>{throw new Error('Unexpected binding switch')}
        const unregister=registry.registerSyncBindingTransport(target,{
            inspectTarget:unsupported,pullAvailableState:unsupported,replaceFromTarget:unsupported,publishInitialSharedState:unsupported,
            fenceOldJobs:async()=>{},resumeBinding:async()=>{throw new Error('Failed authority check must prevent resume')},receiveAvailableChanges:async()=>{},
        })
        const gate=createPluginStartupGate({enabled:()=>true,delay:()=>true,replacementPending:()=>registry.isSyncBindingReplacementPending(),
            context:async()=>{throw new Error('native state unavailable')},start:async()=>{if(await gate.ensure())starts++},
        })
        const stop=subscribeSyncBindingChanges(()=>{if(gate.isWaiting())gate.retry()})
        plugins.fencePluginExecutionForAuthorityReplacement.mockImplementationOnce(async()=>{unavailable=true;throw new Error('preparation failed')})
        plugins.restartPluginsAfterAuthorityReplacement.mockImplementation(async()=>{expect(await gate.ensure()).toBe(false)})
        const h=await createHarness()
        try {
            await expect(h.runtime.replacePersistentDatabase(makeDatabase(),'test',{upstreamImport:true})).rejects.toThrow('preparation failed')
            expect(h.runtime.revision).toBe(1)
            expect(registry.isSyncBindingReplacementPending()).toBe(false)
            gate.loadOnce()
            await vi.waitFor(()=>expect(starts).toBe(1))
        } finally {stop();unregister()}
    })
    it.each(['server','external'] as const)('waits for resumed %s receive before starting plugins on the replacement database',async kind=>{
        const registry=await vi.importActual<typeof import('./sync/bindingRegistry')>('./sync/bindingRegistry')
        const target={kind,connectionId:'synthetic-import'}
        const state={target,targetAuthority:'1',selectionEpoch:'1',libraryId:'synthetic',progress:null}
        bindingBoundary.invoke.mockImplementation(async command=>{if(command==='pds_lww_binding_state')return state;throw new Error(command)})
        bindingBoundary.prepare.mockImplementation(registry.prepareBoundLibraryReplacement)
        let active=true,resumed=false,starts=0
        let finish!:()=>void
        const received=new Promise<void>(resolve=>{finish=resolve})
        const unsupported=async():Promise<never>=>{throw new Error('Unexpected binding switch')}
        const transport={inspectTarget:unsupported,pullAvailableState:unsupported,replaceFromTarget:unsupported,publishInitialSharedState:unsupported,
            fenceOldJobs:async()=>{active=false},resumeBinding:async()=>{active=true;resumed=true},
            receiveAvailableChanges:async()=>{if(!active)throw new Error(kind==='server'?'Sync binding changed':'Sync is stopped');if(resumed)await received},
        }
        const unregister=registry.registerSyncBindingTransport(target,transport)
        const h=await createHarness()
        const gate=createPluginStartupGate({enabled:()=>true,delay:()=>true,
            context:async()=>({identity:'synthetic',target:kind,bound:true,replacementPending:registry.isSyncBindingReplacementPending(state),receive:transport.receiveAvailableChanges,assertCurrent:async()=>{}}),
            start:async()=>{if(await gate.ensure()){expect(h.database.username).toBe('replacement');starts++}},
        })
        const stop=subscribeSyncBindingChanges(()=>{if(gate.isWaiting())gate.retry()})
        plugins.restartPluginsAfterAuthorityReplacement.mockImplementation(async()=>{
            expect(await gate.ensure()).toBe(false)
            expect(get(gate.state).phase).toBe('waiting')
        })
        try {
            await expect(h.runtime.replacePersistentDatabase({...makeDatabase(),username:'replacement'},'test',{upstreamImport:true})).resolves.toMatchObject({revision:2})
            expect(starts).toBe(0)
            expect(get(gate.state).phase).toBe('waiting')
            expect(registry.isSyncBindingReplacementPending(state)).toBe(false)
            finish()
            await vi.waitFor(()=>expect(starts).toBe(1))
            expect(get(gate.state).phase).toBe('released')
        } finally {finish();stop();unregister()}
    })
    it.each(['preparation','unchanged-write'] as const)('restarts plugins after %s failure even when binding verification is unavailable',async failure=>{
        const h=await createHarness()
        if(failure==='preparation') plugins.fencePluginExecutionForAuthorityReplacement.mockRejectedValueOnce(new Error('preparation failed'))
        else vi.mocked(h.store.replaceFromDatabase).mockRejectedValueOnce(new Error('unchanged write failed'))
        binding.assertAuthority.mockImplementation(async()=>{throw new Error('binding unavailable')})
        if(failure==='unchanged-write') binding.assertAuthority.mockResolvedValueOnce(undefined)
        try {
            await expect(h.runtime.replacePersistentDatabase(makeDatabase(),'failed-import',{upstreamImport:true})).rejects.toThrow()
            expect(h.runtime.revision).toBe(1)
            expect(h.database).toEqual(h.durable)
            expect(plugins.restartPluginsAfterAuthorityReplacement).toHaveBeenCalledOnce()
            expect(binding.resume).not.toHaveBeenCalled()
        } finally { binding.assertAuthority.mockImplementation(async()=>{}) }
    })
    it('keeps first-run consent through the compatibility replacement runtime',async()=>{
        binding.bound=false
        const h=await createHarness()
        await expect(h.runtime.replacePersistentDatabase(makeDatabase(),'local-backup',{upstreamImport:true,firstRun:true})).resolves.toEqual({kind:'committed',revision:2,projection:'applied'})
        expect(alerts.checkbox).not.toHaveBeenCalled()
        expect(binding.fence).toHaveBeenCalledOnce()
        expect(h.store.replaceFromDatabase).toHaveBeenCalledOnce()
    })
    it.each(['cursor','resume'] as const)('shares concurrent recovery after adoption while %s is blocked and retains failed resume',async phase=>{
        const h=await createHarness()
        const incoming={...makeDatabase(),username:'Concurrent import'}
        vi.mocked(h.store.replaceFromDatabase).mockImplementationOnce(async database=>{
            h.nativeCommit(database)
            throw new Error('activation response lost')
        })
        await expect(h.runtime.replacePersistentDatabase(incoming,'concurrent-import',{upstreamImport:true})).rejects.toThrow()
        const originalEpoch=h.runtime.getStorageAuthorityEpoch()
        let enter!:()=>void
        let release!:()=>void
        const entered=new Promise<void>(resolve=>{enter=resolve})
        const blocked=new Promise<void>(resolve=>{release=resolve})
        if(phase==='cursor')vi.mocked(h.store.commitWorkingSetChangeCursor!).mockImplementationOnce(async()=>{enter();await blocked})
        binding.resume.mockImplementationOnce(async()=>{
            if(phase==='resume'){enter();await blocked}
            throw new Error('first adapter resume failed')
        })
        const first=h.runtime.retryCommittedWorkingSetRefresh()
        await entered
        expect(h.runtime.getStorageAuthorityEpoch()).toBeGreaterThan(originalEpoch)
        const competing=retryCommittedWorkingSetRefreshWithContinuation(h.runtime)
        release()
        await expect(first).resolves.toMatchObject({projection:'refresh-required'})
        await expect(competing).resolves.toMatchObject({projection:'refresh-required'})
        expect(binding.resume).toHaveBeenCalledOnce()
        expect(()=>h.runtime.markPersistentDataDirty(1)).toThrow()
        await expect(retryCommittedWorkingSetRefreshWithContinuation(h.runtime)).resolves.toEqual({kind:'committed',revision:2,projection:'applied'})
        expect(binding.resume).toHaveBeenCalledTimes(2)
        expect(plugins.restartPluginsAfterAuthorityReplacement).toHaveBeenCalledOnce()
        expect(h.store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(h.database.username).toBe(incoming.username)
        await expect(retryCommittedWorkingSetRefreshWithContinuation(h.runtime)).resolves.toBeNull()
    })

    it('cancels a declined upstream replacement as an abort before fencing the sync binding',async()=>{
        const h=await createHarness()
        alerts.checkbox.mockResolvedValueOnce({confirmed:false,checked:false})
        await expect(h.runtime.replacePersistentDatabase({...makeDatabase(),username:'Declined'},'declined',{upstreamImport:true})).rejects.toMatchObject({name:'AbortError'})
        expect(alerts.checkbox).toHaveBeenCalledOnce()
        expect(binding.fence).not.toHaveBeenCalled()
        expect(h.store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(h.database.username).not.toBe('Declined')
    })
    it.each(['confirmation','before-commit'] as const)('preserves the library when import cancellation arrives during %s',async stage=>{
        const h=await createHarness()
        const controller=new AbortController()
        if(stage==='confirmation') alerts.checkbox.mockImplementationOnce(async()=>{
            controller.abort()
            return {confirmed:true,checked:true}
        })
        else binding.assertAuthority.mockImplementationOnce(async()=>{controller.abort()})
        await expect(restoreNativeOfficialAccountCandidate(h.runtime,
            {...makeDatabase(),username:'Cancelled'},1,controller.signal,[],
        )).rejects.toMatchObject({name:'AbortError'})
        expect((await h.store.readRoot()).value.username).not.toBe('Cancelled')
        expect(h.runtime.revision).toBe(1)
        expect(h.runtime.pendingWorkingSetRefreshRevision).toBeNull()
    })
    it.each([true,false])('replaces a library that holds no character without asking, bound=%s',async bound=>{
        binding.bound=bound
        library.hasCharacters.mockResolvedValue(false)
        const h=await createHarness()
        await expect(h.runtime.replacePersistentDatabase({...makeDatabase(),username:'Imported'},'empty-library',{upstreamImport:true})).resolves.toEqual({kind:'committed',revision:2,projection:'applied'})
        expect(alerts.checkbox).not.toHaveBeenCalled()
        expect(binding.fence).toHaveBeenCalledOnce()
        expect(h.database.username).toBe('Imported')
    })

    it('emits the operation restart receipt only after the required native restart succeeds',async()=>{
        const h=await createHarness()
        const receipt=vi.fn(()=>{expect(plugins.restartPluginsAfterAuthorityReplacement).toHaveBeenCalledOnce()})
        await h.runtime.replacePersistentDatabase(makeDatabase(),'receipt',{upstreamImport:true,onPluginsRestarted:receipt})
        expect(receipt).toHaveBeenCalledOnce()
    })
    it.each([{expectedRevision:99},{expectedMutationGeneration:99}])('preserves snapshot expectation before direct native staging: %j',async expectation=>{
        const h=await createHarness()
        await expect(h.runtime.replacePersistentDatabase({...makeDatabase(),username:'Rejected'},'stale-snapshot',{upstreamImport:true,...expectation})).rejects.toThrow()
        expect(h.store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(h.database.username).not.toBe('Rejected')
        expect(()=>h.runtime.markPersistentDataDirty(1)).not.toThrow()
        expect(binding.resume).toHaveBeenCalledOnce()
    })
    it.each([
        ['refresh',false],['uncertain',false],['refresh',true],['uncertain',true],
    ] as const)('retains read-only activation and resumes only after strict recovery: %s, account=%s',async(failure,account)=>{
        const h=await createHarness({pin:async()=>({publish:async()=>{},dispose:async()=>{}})})
        const incoming={...makeDatabase(),username:'Imported after recovery',globalChatVariables:{shared:'1'}}
        if(account) incoming.characters.push(...[
            {type:'group',chaId:'group-a',name:'Group',chats:[]},{type:'character',chaId:'§temp',name:'Room',chats:[]},
        ] as unknown as Database['characters'])
        if(failure==='refresh') h.replaceDatabase.mockImplementationOnce(()=>{throw new Error('refresh failed')})
        else vi.mocked(h.store.replaceFromDatabase).mockImplementationOnce(async database=>{
            h.nativeCommit(database)
            throw new Error('activation result unavailable')
        })
        const operation=account
            ? restoreNativeOfficialAccountCandidate(h.runtime,incoming,1,undefined,[])
            : h.runtime.replacePersistentDatabase(incoming,'upstream-import',{upstreamImport:true,publishOfficial:true})
        if(account) await expect(operation).rejects.toMatchObject({code:'activation-outcome-unresolved',recoveryRequired:true})
        else await expect(operation).rejects.toThrow()
        // The account restore hands over a candidate prepared as an upstream import, without
        // the group and temporary characters a file import leaves out.
        if(account){
            const handed=vi.mocked(h.store.replaceFromDatabase).mock.calls[0][0]
            expect(handed).toMatchObject({explicitGlobalChatVariables:{shared:'1'}})
            expect(handed.characters.map(character=>character.chaId)).toEqual(['char-a'])
        }
        expect(()=>h.runtime.markPersistentDataDirty(1)).toThrow()
        expect(binding.resume).not.toHaveBeenCalled()
        await expect(retryCommittedWorkingSetRefreshWithContinuation(h.runtime)).resolves.toEqual({kind:'committed',revision:2,projection:'applied'})
        expect(h.database.username).toBe('Imported after recovery')
        expect(binding.resume).toHaveBeenCalledOnce()
        expect(h.runtime.hasPendingOfficialPublication()).toBe(!account)
        await h.runtime.flushPendingDataLocally('recovery-baselines')
        expect(h.store.commit).not.toHaveBeenCalled()
    })
    it.each([true,false])('uses captured LWW authority and preserves explicit official publication=%s',async publishOfficial=>{
        const publication={publish:vi.fn(async()=>{}),dispose:vi.fn(async()=>{})}
        const pin=vi.fn(async()=>publication)
        const h=await createHarness({pin})
        const incoming={...makeDatabase(),username:'Upstream import'}
        await expect(h.runtime.replacePersistentDatabase(incoming,'upstream-import',{upstreamImport:true,publishOfficial})).resolves.toEqual({kind:'committed',revision:2,projection:'applied'})
        expect(h.store.replaceFromDatabase).toHaveBeenCalledWith(expect.objectContaining({username:'Upstream import'}),1,[],undefined,
            {bindingAuthority:'9007199254740993',requestId:expect.any(String)})
        expect(binding.fence).toHaveBeenCalledOnce()
        expect(binding.resume).toHaveBeenCalledOnce()
        expect(h.database.username).toBe('Upstream import')
        expect(h.runtime.hasPendingOfficialPublication()).toBe(publishOfficial)
        if(publishOfficial){
            await h.runtime.publishCurrentOfficialRevision()
            expect(pin).toHaveBeenCalledExactlyOnceWith(2)
            expect(publication.publish).toHaveBeenCalledOnce()
        }else expect(pin).not.toHaveBeenCalled()
        await h.runtime.flushPendingDataLocally('import-baseline')
        expect(h.store.commit).not.toHaveBeenCalled()
    })
    it('clears stale pending official publication on a later upstream import without that option',async()=>{
        const pin=vi.fn(async()=>({publish:async()=>{},dispose:async()=>{}}))
        const h=await createHarness({pin})
        await h.runtime.replacePersistentDatabase({...makeDatabase(),username:'First'},'first',{upstreamImport:true,publishOfficial:true})
        expect(h.runtime.hasPendingOfficialPublication()).toBe(true)
        await h.runtime.replacePersistentDatabase({...makeDatabase(),username:'Second'},'second',{upstreamImport:true,publishOfficial:false})
        expect(h.runtime.hasPendingOfficialPublication()).toBe(false)
        expect(pin).not.toHaveBeenCalled()
        expect(h.database.username).toBe('Second')
    })
})


describe('native upstream job activation recovery with the real runtime',()=>{
    it('cancels and drains a proven busy finalize rejection before exact unchanged abort and resume',async()=>{
        const h=await createHarness()
        const original=structuredClone(h.database)
        let cancelled=false
        const invoke=vi.fn(async(command:string)=>{
            if(command==='native_file_job_start')return {jobId:'synthetic-busy'}
            if(command==='native_file_job_status')return {state:cancelled?'cancelled':'waitingForInput',phase:cancelled?'cancelled':'awaiting-activation',kind:'restore-block-risu-save'}
            if(command==='native_file_job_finalize')throw {code:'library-operation-busy',message:'library-operation-busy'}
            if(command==='native_file_job_cancel'){cancelled=true;return {outcome:'requested'}}
            if(command==='native_file_job_forget')return true
            throw new Error(`Unexpected native command ${command}`)
        })
        plugins.restartPluginsAfterAuthorityReplacement.mockImplementation(async()=>{
            expect(cancelled).toBe(true)
            expect(h.database).toEqual(original)
        })
        await expect(runNativeBlockRisuSaveRestore(h.runtime,{type:'desktopPath',path:'E:/synthetic/busy.risudat'}, {},
            {isTauri:()=>true,invoke,wait:async()=>{}})).rejects.toMatchObject({code:'library-operation-busy'})
        expect(h.database).toEqual(original)
        expect(h.runtime.revision).toBe(1)
        expect(()=>h.runtime.markPersistentDataDirty(1)).not.toThrow()
        expect(plugins.restartPluginsAfterAuthorityReplacement).toHaveBeenCalledOnce()
        expect(binding.resume).toHaveBeenCalledOnce()
        expect(invoke.mock.calls.filter(([command])=>command==='native_file_job_finalize')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command])=>command==='native_file_job_cancel')).toHaveLength(1)
        await expect(h.runtime.retryCommittedWorkingSetRefresh()).resolves.toBeNull()
    })
    it.each((['observer','finalize-response','status-response','late','inaccessible','restart','resume'] as const)
        .flatMap(failure=>(['wait','load-once','disabled'] as const).map(policy=>({failure,policy}))))('reconciles before strict adoption and resumes once: $failure/$policy',async ({failure,policy})=>{
        const h=await createHarness()
        const registry=await vi.importActual<typeof import('./sync/bindingRegistry')>('./sync/bindingRegistry')
        const target={kind:'server' as const,connectionId:'synthetic-native-recovery'}
        const state={target,targetAuthority:'1',selectionEpoch:'1',libraryId:'synthetic',progress:null}
        bindingBoundary.invoke.mockImplementation(async command=>{if(command==='pds_lww_binding_state')return state;throw new Error(command)})
        bindingBoundary.prepare.mockImplementation(registry.prepareBoundLibraryReplacement)
        let active=true,delay=true,starts=0
        const unsupported=async():Promise<never>=>{throw new Error('Unexpected binding switch')}
        const transport={inspectTarget:unsupported,pullAvailableState:unsupported,replaceFromTarget:unsupported,publishInitialSharedState:unsupported,
            fenceOldJobs:async()=>{active=false},resumeBinding:async()=>{await binding.resume();active=true},
            receiveAvailableChanges:async()=>{if(!active)throw new Error('Sync binding changed')},
        }
        const unregister=registry.registerSyncBindingTransport(target,transport)
        const gate=createPluginStartupGate({enabled:()=>true,delay:()=>delay,replacementPending:()=>registry.isSyncBindingReplacementPending(),
            context:async()=>({identity:'synthetic',bound:true,receive:transport.receiveAvailableChanges,assertCurrent:async()=>{}}),
            start:async()=>{if(await gate.ensure()){expect(h.database.username).toBe('Native staged import');starts++}},
        })
        const stop=subscribeSyncBindingChanges(()=>{if(gate.isWaiting())gate.retry()})
        onTestFinished(()=>{stop();unregister()})
        const incoming={...makeDatabase(),username:'Native staged import'}
        const result={revision:2,sourceBytes:16,sourceSha256:'a'.repeat(64),characterCount:1,presetCount:0,warningCodes:[]}
        let submitted=false
        let finished=false
        let unavailable=false
        const events:string[]=[]
        vi.mocked(h.store.commitWorkingSetChangeCursor!).mockImplementation(async revision=>{events.push(`cursor-${revision}`)})
        plugins.restartPluginsAfterAuthorityReplacement.mockImplementation(async()=>{
            expect(h.database.username).toBe(incoming.username)
            expect(()=>h.runtime.markPersistentDataDirty(1)).toThrow()
            expect(events).toContain('cursor-2')
            await expect(h.runtime.commitPersistentUnitIntent('plugin-initial-setter',[{type:'set',key:JSON.stringify(['root','username']),value:'Stale plugin setter'}])).rejects.toThrow()
            expect(await gate.ensure()).toBe(false)
            if(failure==='restart'&&plugins.restartPluginsAfterAuthorityReplacement.mock.calls.length===1)throw new Error('fresh restart failed')
            events.push('restart')
        })
        binding.resume.mockImplementation(async()=>{
            expect(events).toContain('restart')
            expect(()=>h.runtime.markPersistentDataDirty(1)).not.toThrow()
            if(failure==='resume'&&binding.resume.mock.calls.length===1)throw new Error('adapter resume failed')
            events.push('resume')
        })
        const invoke=vi.fn(async(command:string)=>{
            if(command==='native_file_job_start') return {jobId:'synthetic-recovery'}
            if(command==='native_file_job_status'){
                events.push('native-status')
                if(unavailable) throw new Error('status inaccessible')
                if(!submitted) return {state:'waitingForInput',phase:'awaiting-activation',kind:'restore-block-risu-save'} as NativeFileJobStatus
                if(failure==='status-response'&&!finished){finished=true;throw new Error('status response lost')}
                return {state:finished?'succeeded':'running',phase:finished?'complete':'activating-database',kind:'restore-block-risu-save',...(finished?{result}:{})} as NativeFileJobStatus
            }
            if(command==='native_file_job_finalize'){
                submitted=true
                if(failure!=='late'&&failure!=='inaccessible'){h.nativeCommit(incoming);finished=failure!=='status-response'}
                if(failure==='finalize-response'||failure==='late'||failure==='inaccessible'||failure==='restart'){
                    unavailable=failure==='inaccessible'
                    throw new Error('finalize response lost')
                }
                return {outcome:'requested'}
            }
            if(command==='native_file_job_forget') return true
            throw new Error(`Unexpected native command ${command}`)
        })
        const followup=vi.fn(async()=>{})
        await expect(runNativeBlockRisuSaveRestore(h.runtime,{type:'desktopPath',path:'E:/synthetic/upstream.risudat'},
            {afterRefresh:followup,onNativeStatus:async status=>{if((failure==='observer'||failure==='resume')&&status.state==='succeeded')throw new Error('committed observer failed')}},
            {isTauri:()=>true,invoke,wait:async()=>{}})).rejects.toThrow()
        expect(()=>h.runtime.markPersistentDataDirty(1)).toThrow()
        expect(binding.resume).not.toHaveBeenCalled()
        expect(registry.isSyncBindingReplacementPending()).toBe(true)
        if(policy==='load-once')gate.loadOnce()
        if(policy==='disabled')delay=false
        expect(await gate.ensure()).toBe(false)
        expect(starts).toBe(0)
        expect(invoke.mock.calls.filter(([command])=>command==='native_file_job_cancel')).toHaveLength(0)
        const projections=h.replaceDatabase.mock.calls.length
        if(failure==='late'||failure==='inaccessible'){
            await expect(h.runtime.retryCommittedWorkingSetRefresh()).resolves.toMatchObject({projection:'refresh-required'})
            expect(h.replaceDatabase).toHaveBeenCalledTimes(projections)
            expect(plugins.restartPluginsAfterAuthorityReplacement).not.toHaveBeenCalled()
            expect(binding.resume).not.toHaveBeenCalled()
            expect(()=>h.runtime.markPersistentDataDirty(1)).toThrow()
            expect(registry.isSyncBindingReplacementPending()).toBe(true)
            expect(starts).toBe(0)
            unavailable=false
            h.nativeCommit(incoming);finished=true
        }
        if(failure==='restart'||failure==='resume'){
            await expect(h.runtime.retryCommittedWorkingSetRefresh()).resolves.toMatchObject({projection:'refresh-required'})
            expect(()=>h.runtime.markPersistentDataDirty(1)).toThrow()
            expect(followup).not.toHaveBeenCalled()
        }
        await expect(h.runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({kind:'committed',revision:2,projection:'applied'})
        expect(h.database.username).toBe(incoming.username)
        expect(h.runtime.pendingWorkingSetRefreshRevision).toBeNull()
        expect(registry.isSyncBindingReplacementPending()).toBe(false)
        await vi.waitFor(()=>expect(starts).toBe(1))
        expect(plugins.restartPluginsAfterAuthorityReplacement).toHaveBeenCalledTimes(failure==='restart'?2:1)
        expect(binding.resume).toHaveBeenCalledTimes(failure==='resume'?2:1)
        expect(followup).toHaveBeenCalledExactlyOnceWith(true)
        expect(invoke.mock.calls.filter(([command])=>command==='native_file_job_finalize')).toHaveLength(1)
        expect(events.indexOf('cursor-2')).toBeLessThan(events.indexOf('restart'))
        expect(events.indexOf('restart')).toBeLessThan(events.indexOf('resume'))
        expect(invoke.mock.calls.filter(([command])=>command==='native_file_job_forget')).toHaveLength(1)
        await expect(retryCommittedWorkingSetRefreshWithContinuation(h.runtime)).resolves.toBeNull()
    })
})
