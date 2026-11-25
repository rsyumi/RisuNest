import type { LuaEngine } from 'wasmoon'

// risunestCheckLuaSyntax: compiles trigger Lua the way the host loads it, without running it.

export type RisunestLuaSyntaxResult =
    | { ok: true }
    | { ok: false; message: string; line?: number }

type LuaScriptLines = typeof import('../process/luaScriptLines')

// One compile holds the main thread for its whole length.
const MAX_SOURCE_LENGTH = 1_000_000

/** Counts lines the way Lua's lexer does: \n, \r, \r\n and \n\r each end one line. */
function luaLineCount(source: string): number {
    let lines = 1
    for (let index = 0; index < source.length; index++) {
        const code = source.charCodeAt(index)
        if (code !== 10 && code !== 13) continue
        lines++
        const next = source.charCodeAt(index + 1)
        if ((next === 10 || next === 13) && next !== code) index++
    }
    return lines
}

function compile(engine: LuaEngine, source: string, chunk: string, lines: LuaScriptLines): RisunestLuaSyntaxResult {
    // Loads the chunk on its own thread the way LuaExecutionBudget.runSource does, and never calls it.
    const thread = engine.global.newThread()
    const threadIndex = engine.global.getTop()
    try {
        thread.loadString(chunk)
        return { ok: true }
    } catch (error) {
        // A chunk Lua refused leaves its message on the thread; any other failure is not a verdict.
        if (thread.getTop() === 0) throw error
        const compilerMessage = error instanceof Error ? error.message : String(error)
        const message = lines.scriptLineNumbers(compilerMessage)
        const reported = lines.scriptLineOfMessage(compilerMessage)
        // The wrapper ends the script with a newline, so an error at the end of the text names the line after it.
        const lastLine = luaLineCount(source)
        const line = reported === lastLine + 1 ? lastLine : reported
        return line !== undefined && line <= lastLine ? { ok: false, message, line } : { ok: false, message }
    } finally {
        try {
            thread.close()
        } finally {
            engine.global.remove(threadIndex)
        }
    }
}

/** Answers checks one at a time in arrival order, on the engine `loadEngine` creates for the first one. */
export function createRisunestLuaSyntaxChecker(loadEngine: () => Promise<LuaEngine>) {
    let engine: Promise<LuaEngine> | undefined
    let previous: Promise<unknown> = Promise.resolve()

    async function check(source: unknown): Promise<RisunestLuaSyntaxResult> {
        if (typeof source !== 'string') throw new TypeError('source must be a string')
        if (source.length > MAX_SOURCE_LENGTH) {
            throw new RangeError(`source must be at most ${MAX_SOURCE_LENGTH} UTF-16 code units`)
        }
        const pending = engine ??= loadEngine()
        let loaded: LuaEngine
        try {
            loaded = await pending
        } catch (error) {
            if (engine === pending) engine = undefined
            throw error
        }
        const [{ luaCodeWrapper }, lines] = await Promise.all([
            import('../process/luaCodeWrapper'),
            import('../process/luaScriptLines'),
        ])
        return compile(loaded, source, luaCodeWrapper(source), lines)
    }

    return (source?: unknown): Promise<RisunestLuaSyntaxResult> => {
        const result = previous.then(() => check(source))
        previous = result.catch(() => undefined)
        return result
    }
}

async function createCheckEngine(): Promise<LuaEngine> {
    const { createLuaFactory } = await import('../process/luaRuntime')
    const factory = await createLuaFactory()
    // Nothing runs on this engine, so it gets no libraries and no host functions.
    return await factory.createEngine({ openStandardLibs: false, injectObjects: false, enableProxy: false })
}

let defaultCheck: ReturnType<typeof createRisunestLuaSyntaxChecker> | undefined

export function checkRisunestLuaSyntax(source?: unknown): Promise<RisunestLuaSyntaxResult> {
    defaultCheck ??= createRisunestLuaSyntaxChecker(createCheckEngine)
    return defaultCheck(source)
}
