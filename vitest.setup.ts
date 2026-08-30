import { vi } from 'vitest'
import rfdc from 'rfdc'

// Suppress warning
vi.mock(import('katex'), () => ({}))

// Mirror the production safeStructuredClone from src/ts/polyfill.ts (structuredClone
// with an rfdc fallback) instead of importing polyfill.ts, which would pull its
// drag-drop/stream/global side effects into every suite. A JSON round-trip must not
// be used here: it silently changes clone semantics (drops undefined/function
// properties, stringifies Dates, mangles typed arrays, throws on cycles) in the
// data-loss-critical storage suites.
const rfdcClone = rfdc({
  circles: false,
})
vi.stubGlobal('safeStructuredClone', <T>(data: T): T => {
  try {
    return structuredClone(data)
  } catch (error) {
    return rfdcClone(data)
  }
})
