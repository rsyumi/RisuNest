## Task 12 report: B2 peer residues and replacement alias provenance

### Scope

Task 12 reclaims peer clone residue only when existing persistent evidence proves ownership and activation, integrates the existing P4 database and directory startup sweeps in one crash regression, and adds generation-scoped provenance for aliases forwarded by future database-only replacements.

No peer protocol, logical delta record contract, CAS authority, cold payload authority, or activation marker contract changed.

### RED evidence

- `successful_source_stop_sweeps_only_canonical_markerless_session_directories` initially failed because a canonical markerless source session directory remained after a successful stop.
- `activated_target_residue_is_reclaimed_after_process_restart` initially failed because the exact finalized target job directory remained after recreating command state.
- `schema_v16_adds_generation_scoped_exact_replacement_alias_provenance` initially observed schema version 15 instead of 16 because no durable provenance table existed.
- `database_only_replace_prunes_only_exact_forwarded_alias_candidates_after_complete_scan` initially found the unreachable forwarded alias still present because replacements had no exact forwarding provenance.
- The integrated crashed P4 staging regression was GREEN before production changes. Existing `PersistentStore::open` recovery and the existing strict directory sweep already composed correctly, so no duplicate sweeper was added.

### GREEN evidence

- Peer command lifecycle: `cargo test --manifest-path src-tauri/Cargo.toml peer_sync::commands::tests --lib -- --test-threads=1`, 25 passed.
- Replacement behavior: `cargo test --manifest-path src-tauri/Cargo.toml database_only_replace_ --lib -- --test-threads=1`, 6 passed.
- Startup sweeps: `cargo test --manifest-path src-tauri/Cargo.toml persistent_store::commands::tests --lib -- --test-threads=1`, 7 passed.
- Schema and migrations: `cargo test --manifest-path src-tauri/Cargo.toml schema --lib -- --test-threads=1`, 33 passed. The v15 to v16 regression also passed separately after verifying an existing sentinel survived migration.
- Asset GC: `cargo test --manifest-path src-tauri/Cargo.toml asset_gc_ --lib -- --test-threads=1`, 17 passed. `historical_alias_without_replacement_provenance_remains_a_gc_root` also passed separately.
- Broad persistent store: `cargo test --manifest-path src-tauri/Cargo.toml persistent_store:: --lib -- --test-threads=1`, 309 passed, 2 ignored, 0 failed.
- Formatting: `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`, passed.
- Rust check: `cargo check --manifest-path src-tauri/Cargo.toml`, passed with existing dead-code warnings.
- Svelte and TypeScript check: `pnpm check`, passed with 0 errors and 0 warnings.

Every automated command above ran with `VITE_DISABLE_REALM=true`.

### Peer cleanup evidence

Target startup cleanup first reads the authoritative current revision and `peerCloneActiveManifest` value from the persistent store. It accepts only the exact two-field marker whose revision equals the current revision and whose manifest ID is a canonical SHA-256 hash. A target directory is deleted only when all of these predicates hold:

- the peer root, targets root, canonical UUID job directory, and transfer directory are ordinary directories;
- `manifest.json` is a bounded ordinary file, not a symlink or reparse point;
- the manifest parses, validates, and is already in canonical byte form;
- the manifest session ID equals the canonical job directory name;
- the manifest identity exactly equals the active persistent marker identity.

Missing, malformed, stale, ambiguous, noncanonical, linked, junction, reparse, or non-directory entries are retained.

Source cleanup runs the markerless canonical directory sweep only after runtime shutdown and the current session cleanup succeed. Cleanup failures remain retryable. The sweep removes only lower-case canonical UUID ordinary directories beneath the verified ordinary `source-sessions` root. Files, noncanonical names, symlinks, junctions, reparse points, and their targets are retained.

The P4 crash regression creates both abandoned database staging rows and a canonical directory residue, reopens the store, runs the existing directory sweep, and proves the active revision and root remain intact while every generation table row for the abandoned staging ID and the canonical owned directory are gone.

Ruling: Persistent activation evidence, not directory shape alone, authorizes target residue deletion.

Ruling: A successful source stop authorizes cleanup of canonical markerless owned source session directories, while every ambiguous filesystem entry is retained.

Ruling: Existing P4 database and directory sweepers remain the only production sweep authorities.

### Alias provenance and pruning evidence

Schema v16 adds `asset_alias_replacement_candidates`, keyed by generation, kind, logical key, and exact object hash, with the forwarded byte size recorded. The table participates in ordinary generation copy, move, deletion, snapshot, recovery, and local P4 generation lifecycle. It is local persistent metadata and does not add a logical delta wire record.

Only a future database-only replacement creates candidate rows. It first forwards the active aliases, then records exact non-null hash and size provenance for those forwarded rows. Ordinary historical aliases receive no retroactive provenance.

Pruning runs in the same replacement transaction. It aborts pruning unless the supported replacement owner shapes are complete and well-typed. Any plugin storage or cold alias row makes the scan opaque and retains all forwarded aliases. Otherwise it recursively scans root, presets, characters, conversations, and messages, plus preset and character image fields, for exact asset and inlay logical-key references. An alias is removed only when:

- the exact generation-scoped candidate exists;
- the complete non-opaque scan found no reference to its logical key;
- the current alias still matches candidate kind, logical key, object hash, and byte size.

The candidate evidence remains after alias removal for auditability. GC no longer sees a pruned alias as a current root, but historical aliases without provenance and aliases under opaque ownership remain roots.

Ruling: Provenance applies prospectively to aliases forwarded by future database-only replacements and never retroactively classifies historical aliases.

Ruling: Only exact candidates proven unreachable by a complete non-opaque scan are pruned.

Ruling: Historical aliases, malformed owner shapes, plugin-owned ambiguity, cold payload ambiguity, and any exact-match uncertainty are retained.

### Risks and limits

- Filesystem proof is intentionally fail-closed. Interrupted or malformed residues can remain for later diagnosis instead of risking deletion.
- Source cleanup assumes the existing single command-state ownership model. It does not introduce cross-process locking or a new lifecycle authority.
- Plugin and cold payload presence conservatively disables alias pruning for that replacement. This can retain bytes, but it prevents deletion where the supported scan cannot prove reachability.
- Aliases with no object hash are not provenance candidates because they do not identify CAS bytes. They remain preserved.
- Existing dead-code warnings remain unchanged. Live RisuRealm validation was intentionally skipped under the required test isolation flag.

Ruling: Conservative retention is the required outcome whenever proof is incomplete or opaque.
