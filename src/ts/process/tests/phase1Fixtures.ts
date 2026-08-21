import type { customscript } from "../../storage/database.svelte"

export type RegexFixtureSize = 20 | 100 | 500

const DEFAULT_TARGET_BYTES = 32 * 1024

const expectedHashes: Record<RegexFixtureSize, string> = {
    20: "5e460344",
    100: "d3272578",
    500: "b61dac41",
}

export function fnv1a(text: string): string {
    let hash = 0x811c9dc5
    for (let index = 0; index < text.length; index++) {
        hash = Math.imul(hash ^ text.charCodeAt(index), 0x01000193)
    }
    return (hash >>> 0).toString(16).padStart(8, "0")
}

export function makeRegexFixture(
    ruleCount: RegexFixtureSize,
    targetBytes = DEFAULT_TARGET_BYTES,
): { scripts: customscript[]; input: string; expectedHash: string } {
    const scripts = Array.from({ length: ruleCount }, (_, index): customscript => {
        const token = index.toString().padStart(3, "0")
        return {
            comment: `phase1 rule ${token}`,
            in: `rule-${token}`,
            out: `done-${token}`,
            type: "editoutput",
            flag: "g",
            ableFlag: true,
        }
    })

    const parts: string[] = []
    let inputLength = 0
    let index = 0
    while (inputLength < targetBytes) {
        const part = `rule-${(index % ruleCount).toString().padStart(3, "0")}|`
        parts.push(part)
        inputLength += part.length
        index++
    }
    const input = parts.join("")

    return {
        scripts,
        input,
        expectedHash: targetBytes === DEFAULT_TARGET_BYTES
            ? expectedHashes[ruleCount]
            : fnv1a(input.replaceAll("rule-", "done-")),
    }
}
