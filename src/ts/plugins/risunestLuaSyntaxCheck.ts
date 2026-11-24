// risunestCheckLuaSyntax: compiles trigger Lua the way the host loads it, without running it.

export type RisunestLuaSyntaxResult =
    | { ok: true }
    | { ok: false; message: string; line?: number }

export async function checkRisunestLuaSyntax(source?: unknown): Promise<RisunestLuaSyntaxResult> {
    void source
    throw new Error('risunestCheckLuaSyntax is not implemented')
}
