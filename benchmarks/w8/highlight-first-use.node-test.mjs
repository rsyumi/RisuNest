import assert from 'node:assert/strict'
import test from 'node:test'

import { discoverBrowserThenCreateProfile } from './highlight-first-use.mjs'

test('browser discovery failure does not create a temporary profile', async () => {
    let profileCreationCount = 0

    await assert.rejects(
        discoverBrowserThenCreateProfile({
            discoverBrowser: async () => {
                throw new Error('browser unavailable')
            },
            createProfile: async () => {
                profileCreationCount += 1
                return 'unexpected-profile'
            },
        }),
        /browser unavailable/,
    )
    assert.equal(profileCreationCount, 0)
})
