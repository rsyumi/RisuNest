import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { Tiktoken } from '@dqbd/tiktoken'
import cl100kBase from '@dqbd/tiktoken/encoders/cl100k_base.json' with { type: 'json' }
import o200kBase from '../../src/etc/o200k_base.json' with { type: 'json' }

const artifacts = {
    cl100k_base: {
        path: new URL('../../node_modules/@dqbd/tiktoken/encoders/cl100k_base.json', import.meta.url),
        data: cl100kBase,
    },
    o200k_base: {
        path: new URL('../../src/etc/o200k_base.json', import.meta.url),
        data: o200kBase,
    },
}

const inputs = [
    { name: 'empty', class: 'empty', input: { kind: 'text', value: '' } },
    { name: 'ascii-word', class: 'ascii', input: { kind: 'text', value: 'hello' } },
    {
        name: 'whitespace-boundaries',
        class: 'whitespace',
        input: { kind: 'text', value: '  leading\tmiddle\nline\r\ntrailing  ' },
    },
    {
        name: 'punctuation-regex-boundaries',
        class: 'punctuation',
        input: { kind: 'text', value: 'Hello?!\n/regex\\/path/; []{}()' },
    },
    {
        name: 'contractions-and-case',
        class: 'contractions',
        input: { kind: 'text', value: "I'm I'M can't CAN'T we'd you're O'Reilly" },
    },
    {
        name: 'numeric-grouping-boundaries',
        class: 'numbers',
        input: { kind: 'text', value: '1 12 123 1234 12345 0001 999999999' },
    },
    {
        name: 'source-code',
        class: 'source-code',
        input: { kind: 'text', value: 'const value = items.map((item) => item.id ?? 0);\n' },
    },
    {
        name: 'json',
        class: 'json',
        input: { kind: 'text', value: '{"name":"Risu","enabled":false,"count":0,"items":[]}' },
    },
    {
        name: 'markdown',
        class: 'markdown',
        input: { kind: 'text', value: '# Heading\n\n- **bold** and `code`\n\n> quote' },
    },
    {
        name: 'url',
        class: 'url',
        input: { kind: 'text', value: 'https://example.test/a%20b?q=hello%2Fworld&empty=#fragment' },
    },
    {
        name: 'base64-like',
        class: 'base64',
        input: { kind: 'text', value: 'VGhpcy1pc19hX2xvbmctYmFzZTY0X2xpa2Vfc3RyaW5nPT0=' },
    },
    { name: 'korean', class: 'korean', input: { kind: 'text', value: '안녕하세요, 리수네스트입니다.' } },
    {
        name: 'simplified-chinese',
        class: 'simplified-chinese',
        input: { kind: 'text', value: '你好，世界。简体中文分词测试。' },
    },
    {
        name: 'traditional-chinese',
        class: 'traditional-chinese',
        input: { kind: 'text', value: '你好，世界。繁體中文分詞測試。' },
    },
    { name: 'japanese', class: 'japanese', input: { kind: 'text', value: 'こんにちは世界。カタカナと漢字。' } },
    {
        name: 'arabic-rtl',
        class: 'arabic-rtl',
        input: { kind: 'text', value: '\u200fمرحبا بالعالم\u200e' },
    },
    { name: 'cyrillic', class: 'cyrillic', input: { kind: 'text', value: 'Привет, мир. Проверка.' } },
    { name: 'devanagari', class: 'devanagari', input: { kind: 'text', value: 'नमस्ते दुनिया। परीक्षण।' } },
    { name: 'unicode-nfc', class: 'unicode-normalization', input: { kind: 'text', value: 'café Å 가' } },
    { name: 'unicode-nfd', class: 'unicode-normalization', input: { kind: 'text', value: 'cafe\u0301 A\u030a 가' } },
    {
        name: 'emoji-zwj-variation-flags-skin',
        class: 'emoji',
        input: { kind: 'text', value: '👨‍👩‍👧‍👦 ❤️ 🇰🇷 👍🏽 ✈️' },
    },
    {
        name: 'nul-and-controls',
        class: 'control-characters',
        input: { kind: 'text', value: '\u0000\u0001\b\u000b\u001f' },
    },
    {
        name: 'spacing-and-replacement-controls',
        class: 'control-characters',
        input: { kind: 'text', value: 'a\u00a0b\u200bc�' },
    },
    {
        name: 'lone-high-surrogate',
        class: 'utf16-boundary',
        input: { kind: 'utf16-code-units', codeUnits: [55296] },
    },
    {
        name: 'lone-low-surrogate',
        class: 'utf16-boundary',
        input: { kind: 'utf16-code-units', codeUnits: [56320] },
    },
    {
        name: 'mixed-surrogate-boundary',
        class: 'utf16-boundary',
        input: { kind: 'utf16-code-units', codeUnits: [65, 55296, 66, 55357, 56842, 56320, 67] },
    },
    {
        name: 'synthetic-system-segment',
        class: 'prompt-segment',
        input: { kind: 'text', value: 'System: Follow the character card and answer in Korean.\nSafety: preserve code fences.' },
    },
    {
        name: 'synthetic-character-segment',
        class: 'prompt-segment',
        input: { kind: 'text', value: 'Name: Mira\nPersonality: curious, concise\nScenario: a rain-soaked library' },
    },
    {
        name: 'synthetic-chat-segment',
        class: 'prompt-segment',
        input: { kind: 'text', value: 'user: Can you explain this?\nassistant: 물론이죠. 첫 단계는 경계를 확인하는 것입니다.' },
    },
    {
        name: 'synthetic-lorebook-segment',
        class: 'prompt-segment',
        input: { kind: 'text', value: '[Lore]\nkey: moon, archive\ncontent: The archive opens only at midnight.' },
    },
    {
        name: 'synthetic-code-block-segment',
        class: 'prompt-segment',
        input: { kind: 'text', value: '```ts\nconst answer: number = 42\nconsole.log(answer)\n```' },
    },
    {
        name: 'synthetic-tool-call-segment',
        class: 'prompt-segment',
        input: { kind: 'text', value: '{"name":"search","arguments":{"query":"RisuNest tokenizer","limit":10}}' },
    },
    {
        name: 'long-prompt-over-32-kib',
        class: 'long-input',
        input: { kind: 'repeat', value: ' ', count: 32768 },
    },
    {
        name: 'allocation-boundary',
        class: 'allocation-boundary',
        input: { kind: 'repeat', value: ' ', count: 4097 },
    },
]

function resolveInput(input) {
    if (input.kind === 'text') return input.value
    if (input.kind === 'utf16-code-units') return String.fromCharCode(...input.codeUnits)
    return input.value.repeat(input.count)
}

function normalizeError(error) {
    const message = error instanceof Error ? error.message : String(error)
    const prefix = 'The text contains a special token that is not allowed: '
    if (!message.startsWith(prefix)) throw error
    return { code: 'disallowed_special_token', token: message.slice(prefix.length) }
}

const cases = []
for (const [tokenizerId, artifact] of Object.entries(artifacts)) {
    const tokenizer = new Tiktoken(
        artifact.data.bpe_ranks,
        artifact.data.special_tokens,
        artifact.data.pat_str,
    )
    try {
        for (const entry of inputs) {
            cases.push({
                name: `${tokenizerId}:${entry.name}`,
                tokenizerId,
                class: entry.class,
                input: entry.input,
                ids: Array.from(tokenizer.encode(resolveInput(entry.input))),
            })
        }
        for (const token of Object.keys(artifact.data.special_tokens)) {
            for (const [suffix, value] of [
                ['alone', token],
                ['embedded', `before ${token} after`],
            ]) {
                try {
                    tokenizer.encode(value)
                    throw new Error(`Expected ${token} to be rejected`)
                } catch (error) {
                    cases.push({
                        name: `${tokenizerId}:special-${token}-${suffix}`,
                        tokenizerId,
                        class: 'special-token-error',
                        input: { kind: 'text', value },
                        error: normalizeError(error),
                    })
                }
            }
        }
    } finally {
        tokenizer.free()
    }
}

const artifactMetadata = {}
for (const [tokenizerId, artifact] of Object.entries(artifacts)) {
    const sha256 = createHash('sha256').update(await readFile(artifact.path)).digest('hex')
    artifactMetadata[tokenizerId] = {
        sha256,
        fingerprint: `${tokenizerId}:dqbd-1.0.22:${sha256}:tiktoken-rs-0.12.0:contract-1`,
    }
}

const corpus = JSON.stringify(
    {
        schemaVersion: 1,
        oracle: { package: '@dqbd/tiktoken', version: '1.0.22' },
        artifacts: artifactMetadata,
        cases,
    },
    null,
    2,
)

const fixturePath = 'benchmarks/tokenizer/native-tokenizer-corpus.json'
process.stdout.write(`*** Begin Patch\n*** Add File: ${fixturePath}\n`)
for (const line of corpus.split('\n')) process.stdout.write(`+${line}\n`)
process.stdout.write('*** End Patch\n')
