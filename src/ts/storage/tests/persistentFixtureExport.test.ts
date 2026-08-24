import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

import { fixtureDatabase } from './persistentDataFixtures'

describe('persistent fixture export', () => {
    it('keeps the checked-in Rust fixture in sync with fixtureDatabase', () => {
        const fixturePath = resolve(
            process.cwd(),
            'src-tauri',
            'fixtures',
            'persistent-fixture.json',
        )
        // Regenerate with scripts/exportPersistentFixture.ts when this fails.
        expect(JSON.parse(readFileSync(fixturePath, 'utf8'))).toEqual(
            JSON.parse(JSON.stringify(fixtureDatabase)),
        )
    })
})
