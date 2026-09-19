import { expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
const source = readFileSync('src/lib/Others/Onboarding/Onboarding.svelte', 'utf8')

it('uses shared theme tokens for onboarding surfaces, actions, and warnings', () => {
    expect(source).toContain('--o-panel: var(--color-darkbg)')
    expect(source).toContain('--o-warn: var(--color-danger-400)')
    expect(source).toContain('background: var(--color-primary-600)')
    expect(source).not.toMatch(/#272b3d|#2d3148|#2f6fe0|#f6c453|rgba\(246, 196, 83/)
})
