import { RegExpParser } from '@eslint-community/regexpp'
import type { AST } from '@eslint-community/regexpp'
import {
    canExecuteRegexPlanInWorker,
    type RegexExecutionPlan,
    type RegexExecutionPlanEntry,
} from './regexExecutionPlan'

export type RegexSafeAtom =
    | { kind: 'literal'; value: number }
    | { kind: 'class'; ranges: Array<{ start: number; end: number }> }
    | { kind: 'group'; alternatives: RegexSafeAlternative[] }
    | { kind: 'capture'; index: number; alternatives: RegexSafeAlternative[] }
    | { kind: 'repeat'; min: number; max: number; atom: RegexSafeAtom }

type RegexSafeLiteral = Extract<RegexSafeAtom, { kind: 'literal' }>

export interface RegexSafeAlternative {
    atoms: RegexSafeAtom[]
}

export type RegexReplacementToken =
    | { kind: 'literal'; value: string }
    | { kind: 'match' }
    | { kind: 'prefix' }
    | { kind: 'suffix' }
    | { kind: 'capture'; index: number }

export interface RegexSafePlanEntry {
    sourceIndex: number
    global: boolean
    captureCount: number
    patternBytes: number
    replacementBytes: number
    pattern: { alternatives: RegexSafeAlternative[] }
    replacement: RegexReplacementToken[]
}

export interface RegexSafePlan {
    version: 1
    entries: RegexSafePlanEntry[]
}

export type RegexSafePlanClassification =
    | { accepted: true; plan: RegexSafePlan }
    | { accepted: false; category: string; sourceIndex?: number }

export interface RegexSafePlanClassifierOptions {
    minInputBytes?: number
}

class UnsafeRegexError extends Error {
    constructor(readonly category: string) {
        super(category)
    }
}

const parser = new RegExpParser({ ecmaVersion: 2025 })
const allowedFlags = new Set(['g', 'u', 'gu', 'ug'])
const regexNestLimit = 29
const utf8Encoder = new TextEncoder()
const escapedAsciiPunctuation = new Set(
    Array.from('!"#$%&\'()*+,-./:;<=>?@[\\]^`{|}~'),
)

interface LoweringState {
    captureCount: number
}

function lowerCharacter(character: AST.Character): RegexSafeLiteral {
    if (character.value > 0x7f) {
        throw new UnsafeRegexError('regex_safe_non_ascii_character')
    }
    if (character.raw.length === 1 && character.raw.charCodeAt(0) === character.value) {
        return { kind: 'literal', value: character.value }
    }
    if (
        character.raw.length === 2
        && character.raw[0] === '\\'
        && escapedAsciiPunctuation.has(character.raw[1])
    ) {
        return { kind: 'literal', value: character.value }
    }
    throw new UnsafeRegexError('regex_safe_escape')
}

function lowerClass(characterClass: AST.CharacterClass): RegexSafeAtom {
    if (characterClass.negate || characterClass.unicodeSets) {
        throw new UnsafeRegexError('regex_safe_character_class')
    }
    const ranges: Array<{ start: number; end: number }> = []
    for (const element of characterClass.elements) {
        if (element.type === 'Character') {
            const literal = lowerCharacter(element)
            ranges.push({ start: literal.value, end: literal.value })
            continue
        }
        if (element.type === 'CharacterClassRange') {
            const min = lowerCharacter(element.min)
            const max = lowerCharacter(element.max)
            ranges.push({ start: min.value, end: max.value })
            continue
        }
        throw new UnsafeRegexError('regex_safe_character_class')
    }
    if (ranges.length === 0) {
        throw new UnsafeRegexError('regex_safe_empty_class')
    }
    return { kind: 'class', ranges }
}

function alternativeIsNullable(alternative: RegexSafeAlternative): boolean {
    return alternative.atoms.every(atomIsNullable)
}

function atomIsNullable(atom: RegexSafeAtom): boolean {
    switch (atom.kind) {
        case 'literal':
        case 'class':
            return false
        case 'group':
        case 'capture':
            return atom.alternatives.some(alternativeIsNullable)
        case 'repeat':
            return atom.min === 0 || atomIsNullable(atom.atom)
    }
}

function lowerAlternatives(
    alternatives: AST.Alternative[],
    state: LoweringState,
    insideQuantifier: boolean,
    depth: number,
): RegexSafeAlternative[] {
    return alternatives.map((alternative) => {
        if (alternative.elements.length === 0) {
            throw new UnsafeRegexError('regex_safe_empty_alternative')
        }
        return {
            atoms: alternative.elements.map((element) => (
                lowerElement(element, state, insideQuantifier, depth)
            )),
        }
    })
}

function lowerGroup(
    group: AST.Group | AST.CapturingGroup,
    state: LoweringState,
    insideQuantifier: boolean,
    depth: number,
): RegexSafeAtom {
    if (depth >= regexNestLimit) {
        throw new UnsafeRegexError('regex_safe_nest_limit')
    }
    if (group.type === 'Group' && group.modifiers !== null) {
        throw new UnsafeRegexError('regex_safe_inline_modifier')
    }
    if (group.type === 'CapturingGroup' && group.name !== null) {
        throw new UnsafeRegexError('regex_safe_named_capture')
    }
    if (group.type === 'CapturingGroup' && insideQuantifier) {
        throw new UnsafeRegexError('regex_safe_capture_under_repeat')
    }
    const index = group.type === 'CapturingGroup' ? ++state.captureCount : undefined
    const alternatives = lowerAlternatives(group.alternatives, state, insideQuantifier, depth + 1)
    if (alternatives.some(alternativeIsNullable)) {
        throw new UnsafeRegexError('regex_safe_nullable_group')
    }
    if (index !== undefined) {
        return { kind: 'capture', index, alternatives }
    }
    return { kind: 'group', alternatives }
}

function lowerElement(
    element: AST.Element,
    state: LoweringState,
    insideQuantifier: boolean,
    depth: number,
): RegexSafeAtom {
    switch (element.type) {
        case 'Character':
            return lowerCharacter(element)
        case 'CharacterClass':
            return lowerClass(element)
        case 'Group':
        case 'CapturingGroup':
            return lowerGroup(element, state, insideQuantifier, depth)
        case 'Quantifier': {
            if (
                insideQuantifier
                || depth >= regexNestLimit
                || !element.greedy
                || !Number.isFinite(element.max)
                || element.min < 0
                || element.min > element.max
                || element.max > 64
            ) {
                throw new UnsafeRegexError('regex_safe_quantifier')
            }
            const atom = lowerElement(element.element, state, true, depth + 1)
            return { kind: 'repeat', min: element.min, max: element.max, atom }
        }
        default:
            throw new UnsafeRegexError('regex_safe_ast_node')
    }
}

function pushLiteral(tokens: RegexReplacementToken[], value: string): void {
    if (value === '') {
        return
    }
    const previous = tokens.at(-1)
    if (previous?.kind === 'literal') {
        previous.value += value
    }
    else {
        tokens.push({ kind: 'literal', value })
    }
}

function isAsciiSource(value: string): boolean {
    for (let index = 0; index < value.length; index++) {
        if (value.charCodeAt(index) > 0x7f) {
            return false
        }
    }
    return true
}

function isWellFormedUtf16(value: string): boolean {
    for (let index = 0; index < value.length; index++) {
        const unit = value.charCodeAt(index)
        if (unit >= 0xd800 && unit <= 0xdbff) {
            const next = value.charCodeAt(index + 1)
            if (index + 1 >= value.length || next < 0xdc00 || next > 0xdfff) {
                return false
            }
            index++
        }
        else if (unit >= 0xdc00 && unit <= 0xdfff) {
            return false
        }
    }
    return true
}

export function tokenizeRegexReplacement(
    replacement: string,
    captureCount: number,
): RegexReplacementToken[] {
    const tokens: RegexReplacementToken[] = []
    let literalStart = 0
    let index = 0
    while (index < replacement.length) {
        if (replacement[index] !== '$' || index + 1 >= replacement.length) {
            index++
            continue
        }
        pushLiteral(tokens, replacement.slice(literalStart, index))
        const next = replacement[index + 1]
        if (next === '$') {
            pushLiteral(tokens, '$')
            index += 2
        }
        else if (next === '&') {
            tokens.push({ kind: 'match' })
            index += 2
        }
        else if (next === '`') {
            tokens.push({ kind: 'prefix' })
            index += 2
        }
        else if (next === "'") {
            tokens.push({ kind: 'suffix' })
            index += 2
        }
        else if (next >= '0' && next <= '9') {
            let captureIndex: number | undefined
            let consumed = 0
            const second = replacement[index + 2]
            if (second >= '0' && second <= '9') {
                const twoDigitIndex = Number(next) * 10 + Number(second)
                if (twoDigitIndex > 0 && twoDigitIndex <= captureCount) {
                    captureIndex = twoDigitIndex
                    consumed = 3
                }
            }
            if (captureIndex === undefined && next !== '0' && Number(next) <= captureCount) {
                captureIndex = Number(next)
                consumed = 2
            }
            if (captureIndex !== undefined) {
                tokens.push({ kind: 'capture', index: captureIndex })
                index += consumed
            }
            else {
                pushLiteral(tokens, '$')
                index++
            }
        }
        else {
            pushLiteral(tokens, '$')
            index++
        }
        literalStart = index
    }
    pushLiteral(tokens, replacement.slice(literalStart))
    return tokens
}

function lowerEntry(entry: RegexExecutionPlanEntry): RegexSafePlanEntry {
    if (!allowedFlags.has(entry.flags)) {
        throw new UnsafeRegexError('regex_safe_flags')
    }
    if (!isAsciiSource(entry.pattern)) {
        throw new UnsafeRegexError('regex_safe_non_ascii_source')
    }
    const ast = parser.parsePattern(
        entry.pattern,
        0,
        entry.pattern.length,
        { unicode: entry.flags.includes('u'), unicodeSets: false },
    )
    const state: LoweringState = { captureCount: 0 }
    const alternatives = lowerAlternatives(ast.alternatives, state, false, 0)
    if (alternatives.some(alternativeIsNullable)) {
        throw new UnsafeRegexError('regex_safe_nullable_pattern')
    }
    if (state.captureCount > 99) {
        throw new UnsafeRegexError('regex_safe_capture_limit')
    }
    return {
        sourceIndex: entry.sourceIndex,
        global: entry.flags.includes('g'),
        captureCount: state.captureCount,
        patternBytes: utf8Encoder.encode(entry.pattern).byteLength,
        replacementBytes: utf8Encoder.encode(entry.replacement).byteLength,
        pattern: { alternatives },
        replacement: tokenizeRegexReplacement(entry.replacement, state.captureCount),
    }
}

export function classifyRegexSafePlan(
    executionPlan: RegexExecutionPlan,
    input: string,
    options: RegexSafePlanClassifierOptions = {},
): RegexSafePlanClassification {
    if (!isWellFormedUtf16(input)) {
        return { accepted: false, category: 'regex_safe_input_utf16' }
    }
    const inputBytes = utf8Encoder.encode(input).byteLength
    if (inputBytes > 1_048_576) {
        return { accepted: false, category: 'regex_safe_input_limit' }
    }
    if (inputBytes < (options.minInputBytes ?? 0)) {
        return { accepted: false, category: 'regex_safe_input_minimum' }
    }
    if (
        executionPlan.mode !== 'editoutput'
        || !canExecuteRegexPlanInWorker(executionPlan, input)
        || executionPlan.entries.length < 1
        || executionPlan.entries.length > 500
    ) {
        return { accepted: false, category: 'regex_safe_plan' }
    }
    let patternBytes = 0
    let replacementBytes = 0
    for (const entry of executionPlan.entries) {
        if (!isWellFormedUtf16(entry.replacement)) {
            return {
                accepted: false,
                category: 'regex_safe_replacement_utf16',
                sourceIndex: entry.sourceIndex,
            }
        }
        if (entry.compileError !== undefined) {
            return {
                accepted: false,
                category: 'regex_safe_compile_error',
                sourceIndex: entry.sourceIndex,
            }
        }
        const entryPatternBytes = utf8Encoder.encode(entry.pattern).byteLength
        if (entryPatternBytes > 4_096) {
            return {
                accepted: false,
                category: 'regex_safe_pattern_limit',
                sourceIndex: entry.sourceIndex,
            }
        }
        patternBytes += entryPatternBytes
        replacementBytes += utf8Encoder.encode(entry.replacement).byteLength
    }
    if (patternBytes > 65_536) {
        return { accepted: false, category: 'regex_safe_pattern_total_limit' }
    }
    if (replacementBytes > 65_536) {
        return { accepted: false, category: 'regex_safe_replacement_total_limit' }
    }
    const entries: RegexSafePlanEntry[] = []
    for (const entry of executionPlan.entries) {
        try {
            entries.push(lowerEntry(entry))
        }
        catch (error) {
            return {
                accepted: false,
                category: error instanceof UnsafeRegexError
                    ? error.category
                    : 'regex_safe_parse',
                sourceIndex: entry.sourceIndex,
            }
        }
    }
    return { accepted: true, plan: { version: 1, entries } }
}
