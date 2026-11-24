# Native backup fixtures

Generate synthetic historical RisuSave files and account-encryption ciphertext:

```text
pnpm install --frozen-lockfile --ignore-scripts
pnpm run generate <risuai-source-checkout>
```

The source checkout must contain the RisuAI release commits recorded in
`generate.mjs`. No reference application is installed or started. The generator
executes the released save writers using this package's pinned libraries and
the product's WebCrypto encryption helper. It writes deterministic JSON fixtures
under `src-tauri/src/native_file_jobs/legacy_backup/fixtures`.

Native restore tests consume these fixtures to verify decoded data and
authenticated plaintext hashes. All names, database values, keys and payloads
are synthetic.
