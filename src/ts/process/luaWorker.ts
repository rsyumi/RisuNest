import type {
  LuaWorkerHostMessage,
  LuaWorkerRequest,
} from './luaWorkerProtocol'
import { createWasmoonLuaWorkerRuntime } from './luaWorkerRuntime'

interface LuaModuleWorkerScope {
  location: Location
  postMessage(message: LuaWorkerHostMessage): void
  addEventListener(type: 'message', listener: EventListener): void
}

const workerScope = globalThis as unknown as LuaModuleWorkerScope
const runtime = createWasmoonLuaWorkerRuntime({
  async loadJsonLua() {
    const response = await fetch(new URL('/lua/json.lua', workerScope.location.href))
    if (!response.ok) {
      throw new Error(`Lua Worker failed to load json.lua: HTTP ${response.status}`)
    }
    const source = await response.text()
    if (source.length === 0) {
      throw new Error('Lua Worker loaded an empty json.lua')
    }
    return source
  },
  postMessage(message) {
    workerScope.postMessage(message)
  },
})

workerScope.addEventListener('message', ((event: MessageEvent<LuaWorkerRequest>) => {
  runtime.handleMessage(event.data)
}) as EventListener)
