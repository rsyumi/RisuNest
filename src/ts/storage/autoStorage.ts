import localforage from "localforage"
import { isNodeServer } from "src/ts/platform"
import { NodeStorage } from "./nodeStorage"
import { OpfsStorage } from "./opfsStorage"
import { alertStore } from "../alert"

export class AutoStorage{
    isAccount:boolean = false
    private sessionAccountMode:boolean|null = null

    realStorage:LocalForage|NodeStorage|OpfsStorage

    async setItem(key:string, value:Uint8Array):Promise<string|null> {
        await this.Init()
        await this.realStorage.setItem(key, value)
        return null
    }
    async getItem(key:string):Promise<Buffer> {
        await this.Init()
        return await this.realStorage.getItem(key)

    }
    async keys():Promise<string[]>{
        await this.Init()
        return await this.realStorage.keys()

    }
    async removeItem(key:string){
        await this.Init()
        return await this.realStorage.removeItem(key)
    }

    setAccountModeForSession(enabled:boolean):void {
        this.sessionAccountMode = enabled
        this.isAccount = enabled
    }

    async Init(){
        this.isAccount = this.sessionAccountMode
            ?? localStorage.getItem('accountst') === 'able'
        if(!this.realStorage){
            if(isNodeServer){
                console.log("using node storage")
                this.realStorage = new NodeStorage()
                return
            }
            else if(window.navigator?.storage?.getDirectory &&
                    FileSystemFileHandle?.prototype?.createWritable &&
                    localStorage.getItem('opfs_flag!') === "able"){
                console.log("using opfs storage")

                const forage = localforage.createInstance({
                    name: "risunest"
                })

                const i = await forage.getItem("database/database.bin")

                if((!i) || (await forage.getItem("migrated"))){
                    this.realStorage = new OpfsStorage()
                    return
                }
                else if(!(await forage.getItem("denied_opfs"))){
                    console.log("migrating")
                    const keys = await forage.keys()
                    let i = 0;
                    const opfs = new OpfsStorage()
                    for(const key of keys){
                        alertStore.set({
                            type: "wait",
                            msg: `Migrating your data...(${i}/${keys.length})`
                        })
                        await opfs.setItem(key,await forage.getItem(key))
                        i += 1
                    }
                    this.realStorage = opfs
                    alertStore.set({
                        type: "none",
                        msg: ""
                    })
                    await forage.setItem("migrated", true)
                    return
                }
            }
            console.log("using forage storage")
            this.realStorage = localforage.createInstance({
                name: "risunest"
            })
        }
    }

    listItem = this.keys
}
