import { luaCodeWrapper } from './luaCodeWrapper'

let leadingLines: number | undefined

/** Lines the wrapper puts before the script, so reported lines count from the script's first line. */
function luaWrapperLeadingLines(): number {
    return leadingLines ??= luaCodeWrapper('\0').split('\n').findIndex((line) => line.includes('\0'))
}

// The chunk position Lua puts before a message, and the lines its compiler names inside one.
const wrapperLineReference = new RegExp('(' + [
    String.raw`\[string "\.\.\."\]:`,
    String.raw`unfinished long (?:string|comment) \(starting at line `,
    String.raw` expected \(to close '[^'\n]+' at line `,
    String.raw`\(limit is \d+\) in function at line `,
    String.raw`break outside loop at line `,
    String.raw`for <goto> at line `,
    String.raw`<goto [^>\n]+> at line `,
    String.raw`already defined on line `,
].join('|') + String.raw`)(\d+)`, 'g')

/** Rewrites the line numbers in a Lua message about a wrapped script to count from the script's first line. */
export function scriptLineNumbers(text: string): string {
    const leading = luaWrapperLeadingLines()
    return text.replace(wrapperLineReference, (match, reference: string, line: string) => {
        const scriptLine = Number(line) - leading
        return scriptLine > 0 ? reference + scriptLine : match
    })
}

/** The script line a Lua message about a wrapped script starts with, if it names one in the script. */
export function scriptLineOfMessage(message: string): number | undefined {
    const position = /^\[string "\.\.\."\]:(\d+):/.exec(message)
    if (!position) return undefined
    const scriptLine = Number(position[1]) - luaWrapperLeadingLines()
    return scriptLine > 0 ? scriptLine : undefined
}
