import { bench, describe, expect } from 'vitest'
import {
    makeAssetManifestFixture,
    measureAssetManifestFixture,
} from './tests/assetManifestFixtures'

const fixture = makeAssetManifestFixture()
const measurement = measureAssetManifestFixture(fixture)

console.log(`asset-manifest-baseline ${JSON.stringify(measurement)}`)

describe('owner asset tuple baseline', () => {
    bench('stringifies the large owner fixture', () => {
        const serialized = JSON.stringify(fixture)
        expect(Buffer.byteLength(serialized)).toBe(measurement.serializedRootBytes)
    })
})
