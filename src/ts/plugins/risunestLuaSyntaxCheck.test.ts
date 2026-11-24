// @vitest-environment node

import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { expect, test, vi } from 'vitest'
import type { LuaEngine } from 'wasmoon'
import { luaCodeWrapper } from '../process/luaCodeWrapper'
import { createLuaFactory } from '../process/luaRuntime'
import { checkRisunestLuaSyntax, createRisunestLuaSyntaxChecker, type RisunestLuaSyntaxResult } from './risunestLuaSyntaxCheck'

async function withEngine(use: (engine: LuaEngine) => Promise<void>) {
    const engine = await (await createLuaFactory()).createEngine({ openStandardLibs: false })
    try {
        await use(engine)
    } finally {
        engine.global.close()
    }
}

test('compiles a script without running the script or the wrapper around it', async () => {
    const factory = await createLuaFactory()
    await factory.mountFile('json.lua', await readFile(resolve(process.cwd(), 'public/lua/json.lua'), 'utf8'))
    // Unlike the check's own engine, this one can run the wrapper and the script, so a run would show here.
    const engine = await factory.createEngine({ injectObjects: true, enableProxy: false })
    const hostCall = vi.fn()
    engine.global.set('hostCall', hostCall)
    try {
        const check = createRisunestLuaSyntaxChecker(async () => engine)
        await expect(check('checked = true\nhostCall()')).resolves.toStrictEqual({ ok: true })
        expect(engine.global.get('checked')).toBeNull()
        expect(engine.global.get('listenEdit')).toBeNull()
        expect(hostCall).not.toHaveBeenCalled()
    } finally {
        engine.global.close()
    }
})

test.each([
    { name: 'an empty script', source: '' },
    { name: 'a loop that never ends', source: 'while true do end' },
])('accepts $name', async ({ source }) => {
    await expect(checkRisunestLuaSyntax(source)).resolves.toStrictEqual({ ok: true })
})

// The wrapper ends the script with a newline, so an error found at the end of the text is one line past the script.
// Each line ending Lua accepts (\n, \r\n, \r and \n\r) counts as one line.
test.each<{ source: string; result: RisunestLuaSyntaxResult }>([
    {
        source: 'a = 1\nb = 2\nx = = 1',
        result: { ok: false, line: 3, message: `[string "..."]:3: unexpected symbol near '='` },
    },
    {
        source: 'a = 1\nx = "abc\nb = 2',
        result: { ok: false, line: 2, message: `[string "..."]:2: unfinished string near '"abc'` },
    },
    {
        source: 'a = 1\r\n--[[ abc\r\nb = 2',
        result: { ok: false, message: `[string "..."]:4: unfinished long comment (starting at line 2) near <eof>` },
    },
    {
        source: 'a = 1\n\rx = [[ abc',
        result: { ok: false, message: `[string "..."]:3: unfinished long string (starting at line 2) near <eof>` },
    },
    {
        source: 'function f()\n    return 1',
        result: { ok: false, message: `[string "..."]:3: 'end' expected (to close 'function' at line 1) near <eof>` },
    },
    {
        source: 'function f()\n' + Array.from({ length: 201 }, (_, index) => `    local v${index}`).join('\n') + '\nend',
        result: {
            ok: false,
            line: 203,
            message: `[string "..."]:203: too many local variables (limit is 200) in function at line 1 near 'end'`,
        },
    },
    { source: 'x = 1\nbreak', result: { ok: false, message: `[string "..."]:3: break outside loop at line 2` } },
    {
        source: 'goto nowhere\nx = 1',
        result: { ok: false, message: `[string "..."]:3: no visible label 'nowhere' for <goto> at line 1` },
    },
    {
        source: 'goto skip\rlocal x = 1\r::skip::\rx = 2',
        result: { ok: false, line: 4, message: `[string "..."]:4: <goto skip> at line 1 jumps into the scope of local 'x'` },
    },
    { source: '::a::\n::a::', result: { ok: false, message: `[string "..."]:3: label 'a' already defined on line 2` } },
    { source: 'x = ' + '('.repeat(300) + '1' + ')'.repeat(300), result: { ok: false, message: 'C stack overflow' } },
])('reports $result.message', async ({ source, result }) => {
    await expect(checkRisunestLuaSyntax(source)).resolves.toStrictEqual(result)
})

test("counts the wrapper's top-level locals against Lua's limit of 200", async () => {
    const wrapperLocals = luaCodeWrapper('').split('\n')
        .map((line) => /^local function /.test(line) ? 1 : /^local ([^=]+)/.exec(line)?.[1].split(',').length ?? 0)
        .reduce((sum, count) => sum + count, 0)
    expect(wrapperLocals).toBeGreaterThan(0)
    const locals = (count: number) => Array.from({ length: count }, (_, index) => `local v${index} = ${index}`).join('\n')

    await expect(checkRisunestLuaSyntax(locals(200 - wrapperLocals))).resolves.toStrictEqual({ ok: true })
    const over = 200 - wrapperLocals + 1
    await expect(checkRisunestLuaSyntax(locals(over))).resolves.toStrictEqual({
        ok: false,
        line: over,
        message: `[string "..."]:${over}: too many local variables (limit is 200) in main function near '='`,
    })
})

test('rejects a source that is not a string or is longer than 1,000,000 UTF-16 code units', async () => {
    await expect(checkRisunestLuaSyntax(42)).rejects.toThrow(TypeError)
    const longest = '--' + '\u{1F600}'.repeat(499_999)
    expect(longest).toHaveLength(1_000_000)
    await expect(checkRisunestLuaSyntax(longest + 'x')).rejects.toThrow(RangeError)
    await expect(checkRisunestLuaSyntax(longest)).resolves.toStrictEqual({ ok: true })
})

test('rejects while the Lua engine cannot be loaded and loads it for a later call', async () => {
    await withEngine(async (engine) => {
        const loadEngine = vi.fn<() => Promise<LuaEngine>>()
            .mockRejectedValueOnce(new Error('engine unavailable'))
            .mockResolvedValue(engine)
        const check = createRisunestLuaSyntaxChecker(loadEngine)
        await expect(check('x = 1')).rejects.toThrow('engine unavailable')
        await expect(check('x = 1')).resolves.toStrictEqual({ ok: true })
    })
})

test('answers calls in the order they arrive', async () => {
    await withEngine(async (engine) => {
        let provideEngine!: (engine: LuaEngine) => void
        const engineLoaded = new Promise<LuaEngine>((resolve) => { provideEngine = resolve })
        const check = createRisunestLuaSyntaxChecker(() => engineLoaded)
        const answers: unknown[] = []
        const settled = Promise.allSettled(['a = 1', 42, 'x = = 1', ''].map((source) => check(source).then(
            (result) => { answers.push(result) },
            (error: Error) => { answers.push(error.name) },
        )))
        provideEngine(engine)
        await settled
        expect(answers).toEqual([
            { ok: true },
            'TypeError',
            { ok: false, line: 1, message: `[string "..."]:1: unexpected symbol near '='` },
            { ok: true },
        ])
    })
})
