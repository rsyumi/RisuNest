import localforage from 'localforage'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { decodeRisuSave, RisuSaveType } from '../../../risuSave'
import {
    inspectCharXCollisionFixture,
    inspectLocalBackupFixture,
    observeRisuSaveNegativeFixture,
} from './negativeAdapterOracle'

vi.mock('../../../database.svelte', () => ({ presetTemplate: {} }))
vi.mock('../../../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

describe('Roadmap 14 negative adapter oracle', () => {
    beforeEach(async () => {
        await localforage.dropInstance({ name: 'risuSaveCache' })
    })

    it('records stale block-cache completion as a current known gap', async () => {
        const cache = localforage.createInstance({ name: 'risuSaveCache' })
        await cache.setItem('risuSaveBlock_char-stale', {
            type: RisuSaveType.CHARACTER_WITH_CHAT,
            name: 'char-stale',
            data: '{"type":"character","chaId":"char-stale","name":"Stale cache character"}',
        })

        await expect(observeRisuSaveNegativeFixture('stale-block-cache', decodeRisuSave)).resolves.toEqual({
            fixtureId: 'stale-block-cache',
            status: 'known-gap',
            observed: 'missing block was completed from stale local cache',
            warning: 'Block RisuSave may complete a missing required block from stale local cache.',
        })
    })

    it('records an accepted invalid required block as a current known gap', async () => {
        await expect(observeRisuSaveNegativeFixture('invalid-required-block', decodeRisuSave)).resolves.toEqual({
            fixtureId: 'invalid-required-block',
            status: 'known-gap',
            observed: 'invalid required character block was skipped',
            warning: 'Block RisuSave may skip an invalid required block and return partial data.',
        })
    })

    it('freezes truncated-tail and payload-before-database local backup failures', () => {
        expect(inspectLocalBackupFixture('truncated-local-backup-tail')).toEqual({
            entries: ['database.risudat'],
            trailingByteLength: 9,
            payloadBeforeDatabase: false,
            result: {
                status: 'known-gap',
                warning: 'Local backup restore may ignore a truncated nonempty archive tail.',
            },
        })
        expect(inspectLocalBackupFixture('payload-before-database')).toEqual({
            entries: ['avatar.PNG', 'database.risudat'],
            trailingByteLength: 0,
            payloadBeforeDatabase: true,
            result: {
                status: 'known-gap',
                warning: 'Local backup restore may write payloads before database validation and activation.',
            },
        })
    })

    it('freezes mixed-extension CharX collisions and unsafe path aliases', () => {
        expect(inspectCharXCollisionFixture()).toEqual({
            collisionGroups: [[
                'assets/avatar?.PNG',
                'assets/avatar*.png',
            ]],
            extensions: [null, 'png', 'webp'],
            unsafePaths: ['assets/../avatar.png'],
            result: {
                status: 'known-gap',
                warning: 'CharX does not reject every sanitized, case-folded, or traversal path collision.',
            },
        })
    })
})
