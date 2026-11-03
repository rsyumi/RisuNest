import {afterEach, describe, expect, it, vi} from 'vitest'
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule({runtime:'tauri',os:'windows'}))
const core=vi.hoisted(()=>({invoke:vi.fn(async(_command:string,_args?:any):Promise<unknown>=>({}))}))
vi.mock('@tauri-apps/api/core', async (importOriginal) => ({...await importOriginal<typeof import('@tauri-apps/api/core')>(),invoke:core.invoke}))
import {SaveCoordinator} from './saveCoordinator'
import {createLeaseStub,createStoreStub} from './persistentDataStore.testSupport'
import {RevisionConflictError} from './persistentDataStore'
import {makeDatabase} from './saveCoordinator.testSupport'
import {capturePersistentRoot} from './persistentDataRuntime'
import {scheduleDeletedOwnerAssetCleanup} from './deletedOwnerAssetCleanup'
import type {WorkingSetCommit} from './persistentDataStore'

const day=24*60*60*1000
function fixture(stamp: string | undefined, recheck=stamp, archived=false) {
    const database=makeDatabase()
    database.characterOrder=['char-a']
    database.loadouts=[{id:'loadout',characterIds:['char-a','char-b']}] as typeof database.loadouts
    const summary={id:'char-a',name:'Synthetic',type:'character' as const,configuredIndex:0,recentAt:0,trashed:true,conversationCount:0,trashTime:1,trashStampMs:stamp,
        ...(archived ? {archived:{archivedAt:1,conversationCount:1,messageCount:1}} : {})}
    // The native store: commits and alias deletes both expect its revision and advance it.
    const native={revision:1}
    const advance=(expectedRevision:number)=>{
        if(expectedRevision!==native.revision) throw new RevisionConflictError(expectedRevision,native.revision)
        return {revision:++native.revision}
    }
    const commit=vi.fn(async(request:WorkingSetCommit)=>advance(request.expectedRevision))
    core.invoke.mockImplementation(async(command,args)=>command==='pds_asset_delete_orphaned_aliases'?advance(args.expectedRevision):{})
    const release=vi.fn(async()=>{})
    const lease=(revision:number)=>createLeaseStub({revision,readRoot:async()=>({revision,value:capturePersistentRoot(database)}),
        // The native store refuses to read the details an archive holds.
        readCharacter:async(id)=>{if(archived) throw new Error(`Character ${id} is archived`); return {revision,value:{...database.characters[0],chaId:id,trashTime:1,image:'assets/expired.png',
            additionalAssets:[['extra','assets/extra.png','png']]}}},
        readCharacterSummary:async()=>({...summary,trashStampMs:recheck}),
        queryCharacters:async()=>({revision,items:[]}),release,
    })
    const store=createStoreStub({queryCharacters:async()=>({revision:native.revision,items:[summary]}),acquireRevision:async(revision)=>lease(revision),commit})
    const coordinator:SaveCoordinator=new SaveCoordinator({store,captureRoot:()=>capturePersistentRoot(database),captureCharacter:()=>null,captureSelectedCharacter:()=>null,replaceDatabase:()=>{},
        publishCharacterMutation:(result)=>{Object.assign(database,result.root)},
        deletedCharacterAssets:{collectConversationKeys:async()=>['assets/chat.png'],cleanup:(keys,startedAtMs)=>scheduleDeletedOwnerAssetCleanup(keys,startedAtMs,'deleted-owner-assets',()=>({
            runtime:{getStorageAuthorityEpoch:()=>coordinator.storageAuthorityEpoch,flushPendingData:(reason)=>coordinator.flushPendingData(reason),
                runStorageOnlyMutation:(operation)=>coordinator.runStorageOnlyMutation(operation)},
            gate:{runWrite:(operation)=>operation()},
        }))},
    })
    coordinator.initialize(1)
    return {coordinator,commit,release,native}
}

describe('native stamped trash expiry', () => {
    afterEach(()=>core.invoke.mockReset())
    it.each([undefined,String(9*day)])('ignores missing or recent native stamps despite old mutable trashTime: %s', async stamp=>{
        const f=fixture(stamp)
        await expect(f.coordinator.expirePersistentTrash(10*day)).resolves.toBe(0)
        expect(f.commit).not.toHaveBeenCalled()
        expect(core.invoke).not.toHaveBeenCalled()
    })
    it('rechecks the latest pinned stamp before deleting a candidate', async()=>{
        const f=fixture('1',String(9*day))
        await expect(f.coordinator.expirePersistentTrash(10*day)).resolves.toBe(0)
        expect(f.commit).not.toHaveBeenCalled()
        expect(f.release).toHaveBeenCalledOnce()
    })
    it('retires an expired ID and edits only changed order and loadout units', async()=>{
        const f=fixture('1')
        await expect(f.coordinator.expirePersistentTrash(10*day)).resolves.toBe(1)
        expect(f.commit).toHaveBeenCalledExactlyOnceWith({expectedRevision:1,unitMutations:[
            {type:'delete',key:'["exists","character","char-a"]'},
            {type:'set',key:'["order","characters"]',value:[]},
            {type:'set',key:'["record","loadouts","loadout"]',value:{id:'loadout',characterIds:['char-b']}},
        ]})
        // The cleanup reads the library after the delete, and an open lease would still hold its assets.
        await vi.waitFor(()=>expect(core.invoke).toHaveBeenCalledWith('pds_asset_delete_orphaned_aliases',{expectedRevision:2,owner:{
            keys:expect.arrayContaining(['assets/expired.png','assets/extra.png','assets/chat.png']),startedAtMs:expect.any(Number)}}))
        expect(f.release.mock.invocationCallOrder[0]).toBeLessThan(core.invoke.mock.invocationCallOrder[0])
        expect(f.commit.mock.invocationCallOrder[0]).toBeLessThan(core.invoke.mock.invocationCallOrder[0])
    })
    it('adopts the revision the asset cleanup commits, so the next delete expects it', async()=>{
        const f=fixture('1')
        await expect(f.coordinator.deletePersistentCharacters(['char-a'],'purge')).resolves.toBe(1)
        await vi.waitFor(()=>expect(f.native.revision).toBe(3))
        await expect(f.coordinator.deletePersistentCharacters(['char-b'],'purge')).resolves.toBe(1)
        expect(f.commit).toHaveBeenLastCalledWith(expect.objectContaining({expectedRevision:3}))
    })
    it('deletes an archived character with the keys the store reports for it', async()=>{
        const f=fixture('1','1',true)
        await expect(f.coordinator.deletePersistentCharacters(['char-a'],'purge')).resolves.toBe(1)
        expect(f.commit).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({expectedRevision:1,
            unitMutations:expect.arrayContaining([{type:'delete',key:'["exists","character","char-a"]'}])}))
        await vi.waitFor(()=>expect(core.invoke).toHaveBeenCalledWith('pds_asset_delete_orphaned_aliases',{expectedRevision:2,owner:{
            keys:['assets/chat.png'],startedAtMs:expect.any(Number)}}))
    })
    it('does not retry an expired candidate against a newer unchecked revision',async()=>{
        const f=fixture('1')
        f.commit.mockRejectedValueOnce(new RevisionConflictError(1,2))
        await expect(f.coordinator.expirePersistentTrash(10*day)).rejects.toBeInstanceOf(RevisionConflictError)
        expect(f.commit).toHaveBeenCalledOnce()
    })
})
