# Product and verification boundaries

`support/responsesInternals.ts` is installed only by `vitest.config.ts`. It lets
the existing exact Responses assertions inspect private functions in Vitest's
transformed module without changing provider algorithms or product exports.

Check the bundle guard itself:

```sh
node --test tests/productionBundle.node-test.mjs
```

Build and inspect normal application outputs (including dynamic chunks, emitted
worker JS, and source maps) without overwriting `dist`:

```sh
pnpm check:production-bundle desktop
pnpm check:production-bundle android
pnpm check:production-bundle production
```

The check deliberately sets the two removed harness flags to prove that they
cannot enable verification code in a product build. Public assets are not copied.

Standalone tokenizer and streaming harnesses live under `benchmarks/` and consume
product code. `pnpm check:benchmark-harnesses` checks their TypeScript and Svelte.
Their build commands and platform constraints are in the respective READMEs.
