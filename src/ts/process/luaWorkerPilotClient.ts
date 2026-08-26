import {
  LuaWorkerHarnessClient,
  type LuaWorkerHarnessOptions,
  type LuaWorkerLike,
} from './luaWorkerHarness'

export type LuaWorkerPilotClientOptions = Omit<LuaWorkerHarnessOptions, 'workerFactory'>

function createModuleWorker(): LuaWorkerLike {
  return new Worker(new URL('./luaWorker.ts', import.meta.url), {
    type: 'module',
  }) as unknown as LuaWorkerLike
}

export function createLuaWorkerPilotClient(
  options: LuaWorkerPilotClientOptions,
): LuaWorkerHarnessClient {
  return new LuaWorkerHarnessClient({
    ...options,
    workerFactory: createModuleWorker,
    waitForReady: true,
  })
}
