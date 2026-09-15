import { describe, expect, it } from 'vitest'
import { hypaCacheKey, hypaEmbeddingIdentity, type HypaCacheProducer } from './hypaCacheKey'

const CONTENT = '안녕 hello'
const LOCAL = hypaEmbeddingIdentity('MiniLM')

describe('hypaCacheKey', () => {
    it('keeps the documented digest for each producer', async () => {
        const golden: [HypaCacheProducer, string][] = [
            ['hypa-v1-text', '90d485ea9485864b48f7976d56fabf13e622587d5ab43290bd97cf6b25d1da66'],
            ['hypa-v2', 'ab413f028a2e682c38f2a506d0c2f9a360023736022834aa5504a33533c989d8'],
            ['hypa-v3-group', 'c6c8e4332686f0ae7a0c9cbfce58446e319a64e10afbc4455e7b89c8d1f4813c'],
        ]
        for (const [producer, expected] of golden) {
            expect(await hypaCacheKey({ producer, content: CONTENT, identity: LOCAL })).toBe(
                expected,
            )
        }
    })

    it('separates the producers that share the cache', async () => {
        const producers: HypaCacheProducer[] = ['hypa-v1-text', 'hypa-v2', 'hypa-v3-group']
        const keys = await Promise.all(
            producers.map((producer) => hypaCacheKey({ producer, content: CONTENT, identity: LOCAL })),
        )
        expect(new Set(keys).size).toBe(producers.length)
    })

    it('carries the custom server name and URL into the key', async () => {
        const identity = hypaEmbeddingIdentity('custom', ' https://embed.example/v1 ', ' bge-m3 ')
        expect(identity).toEqual({ model: 'custom:bge-m3', endpoint: 'https://embed.example/v1' })
        expect(await hypaCacheKey({ producer: 'hypa-v2', content: CONTENT, identity })).toBe(
            '0948b7f387adf9eeb8fa648de43da4f95cc61e9f0883a7a31e67f11de8b0ee1a',
        )
    })

    it('separates two servers that answer under the same model name', async () => {
        const first = hypaEmbeddingIdentity('custom', 'https://one.example', 'bge-m3')
        const second = hypaEmbeddingIdentity('custom', 'https://two.example', 'bge-m3')
        expect(await hypaCacheKey({ producer: 'hypa-v2', content: CONTENT, identity: first })).not.toBe(
            await hypaCacheKey({ producer: 'hypa-v2', content: CONTENT, identity: second }),
        )
    })

    it('ignores a leftover custom model name while another model is selected', async () => {
        expect(hypaEmbeddingIdentity('nomic', 'https://embed.example', 'bge-m3')).toEqual({
            model: 'nomic',
            endpoint: '',
        })
        expect(await hypaCacheKey({ producer: 'hypa-v2', content: CONTENT, identity: LOCAL })).not.toBe(
            await hypaCacheKey({
                producer: 'hypa-v2',
                content: CONTENT,
                identity: hypaEmbeddingIdentity('nomic'),
            }),
        )
    })

    it('separates a context suffix and folds equivalent unicode forms', async () => {
        expect(
            await hypaCacheKey({
                producer: 'hypa-v2',
                content: CONTENT,
                identity: LOCAL,
                contextSuffix: '|voyageContext3|ctx:abc',
            }),
        ).toBe('cbc6d166c2caa31e60968e0b7859379dbb1b3216f425685e1db2049ca689b42e')

        const decomposed = '한'.normalize('NFD')
        expect(decomposed).not.toBe('한')
        expect(await hypaCacheKey({ producer: 'hypa-v2', content: decomposed, identity: LOCAL })).toBe(
            await hypaCacheKey({ producer: 'hypa-v2', content: '한', identity: LOCAL }),
        )
    })

    it('keeps surrounding whitespace, which is part of the content', async () => {
        expect(await hypaCacheKey({ producer: 'hypa-v2', content: ' a ', identity: LOCAL })).not.toBe(
            await hypaCacheKey({ producer: 'hypa-v2', content: 'a', identity: LOCAL }),
        )
    })

    it('separates a raised preprocess version', async () => {
        expect(
            await hypaCacheKey({ producer: 'hypa-v2', content: CONTENT, identity: LOCAL }),
        ).not.toBe(
            await hypaCacheKey({
                producer: 'hypa-v2',
                content: CONTENT,
                identity: LOCAL,
                preprocessVersion: 2,
            }),
        )
    })

    it('does not let a separator in the content collide with a field boundary', async () => {
        expect(
            await hypaCacheKey({ producer: 'hypa-v2', content: 'a', identity: hypaEmbeddingIdentity('b|c') }),
        ).not.toBe(
            await hypaCacheKey({ producer: 'hypa-v2', content: 'a|b', identity: hypaEmbeddingIdentity('c') }),
        )
    })
})
