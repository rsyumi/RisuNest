pub const SCHEMA: &str = r#"
CREATE TABLE library (singleton INTEGER PRIMARY KEY CHECK(singleton=1), head TEXT NOT NULL);
CREATE TABLE media_secret(singleton INTEGER PRIMARY KEY CHECK(singleton=1),key BLOB NOT NULL CHECK(length(key)=32));
CREATE TABLE devices (
 id TEXT PRIMARY KEY, verifier TEXT NOT NULL UNIQUE, revoked INTEGER NOT NULL DEFAULT 0,
 watermark TEXT NOT NULL DEFAULT '0', ack TEXT NOT NULL DEFAULT '0',
 name TEXT NOT NULL DEFAULT '', registration_request TEXT UNIQUE
);
CREATE TABLE objects (hash TEXT PRIMARY KEY, size INTEGER NOT NULL CHECK(size>=0));
CREATE TABLE object_leases(device TEXT NOT NULL REFERENCES devices(id),hash TEXT NOT NULL REFERENCES objects(hash),expires INTEGER NOT NULL,PRIMARY KEY(device,hash));
CREATE TABLE object_custody(device TEXT NOT NULL REFERENCES devices(id),hash TEXT NOT NULL REFERENCES objects(hash),retention_id TEXT NOT NULL,PRIMARY KEY(device,hash));
CREATE TABLE object_trash(hash TEXT PRIMARY KEY);
CREATE TABLE transfer_recipes(id TEXT PRIMARY KEY,body BLOB NOT NULL,expires INTEGER NOT NULL);
CREATE TABLE read_pins(id TEXT PRIMARY KEY,device TEXT NOT NULL REFERENCES devices(id),after_seq TEXT NOT NULL,through TEXT NOT NULL,expires INTEGER NOT NULL);
CREATE TABLE checkpoints(id TEXT PRIMARY KEY,device TEXT NOT NULL REFERENCES devices(id),head TEXT NOT NULL,expires INTEGER NOT NULL);
CREATE TABLE checkpoint_records(checkpoint TEXT NOT NULL REFERENCES checkpoints(id) ON DELETE CASCADE,key TEXT NOT NULL,version TEXT NOT NULL,PRIMARY KEY(checkpoint,key));
CREATE TABLE uploads (id TEXT PRIMARY KEY,device TEXT NOT NULL REFERENCES devices(id),hash TEXT NOT NULL,size INTEGER NOT NULL,expires INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'open');
CREATE INDEX uploads_device ON uploads(device);
CREATE TABLE upload_jobs(upload TEXT PRIMARY KEY REFERENCES uploads(id) ON DELETE CASCADE,retry_after INTEGER NOT NULL DEFAULT 0,terminal INTEGER NOT NULL DEFAULT 0,error TEXT);
CREATE TABLE upload_deltas(upload TEXT PRIMARY KEY REFERENCES uploads(id) ON DELETE CASCADE,body BLOB NOT NULL);
CREATE TABLE upload_delta_bases(upload TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,hash TEXT NOT NULL,PRIMARY KEY(upload,hash));
CREATE TABLE download_deltas(id TEXT PRIMARY KEY,device TEXT NOT NULL REFERENCES devices(id),request TEXT NOT NULL,expires INTEGER NOT NULL,state TEXT NOT NULL DEFAULT 'queued',body BLOB,error TEXT);
CREATE TABLE download_delta_bases(job TEXT NOT NULL REFERENCES download_deltas(id) ON DELETE CASCADE,hash TEXT NOT NULL,PRIMARY KEY(job,hash));
CREATE TABLE upload_chunks (upload TEXT NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,ordinal INTEGER NOT NULL,hash TEXT NOT NULL,size INTEGER NOT NULL,PRIMARY KEY(upload,ordinal));
CREATE TABLE staging_trash(upload TEXT NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(upload,ordinal));
CREATE TABLE reference_nodes (hash TEXT PRIMARY KEY REFERENCES objects(hash), kind TEXT NOT NULL, item_count INTEGER NOT NULL, tree_depth INTEGER NOT NULL, first_value TEXT NOT NULL, last_value TEXT NOT NULL);
CREATE TABLE reference_children (root TEXT NOT NULL REFERENCES reference_nodes(hash), child TEXT NOT NULL REFERENCES reference_nodes(hash), PRIMARY KEY(root,child));
CREATE TABLE reference_objects (root TEXT NOT NULL REFERENCES reference_nodes(hash), object TEXT NOT NULL REFERENCES objects(hash), PRIMARY KEY(root,object));
CREATE TABLE reference_relations (root TEXT NOT NULL REFERENCES reference_nodes(hash), target TEXT NOT NULL, PRIMARY KEY(root,target));
CREATE TABLE descriptors (hash TEXT PRIMARY KEY REFERENCES objects(hash), object TEXT NOT NULL REFERENCES objects(hash), body TEXT NOT NULL);
CREATE TABLE records (key TEXT PRIMARY KEY, version TEXT NOT NULL);
CREATE TABLE record_relations (source TEXT NOT NULL REFERENCES records(key), target TEXT NOT NULL, PRIMARY KEY(source,target));
CREATE INDEX record_relations_target ON record_relations(target);
CREATE TABLE record_scopes (key TEXT NOT NULL REFERENCES records(key), scope TEXT NOT NULL, PRIMARY KEY(key,scope));
CREATE INDEX record_scopes_members ON record_scopes(scope,key);
CREATE TABLE scope_versions (scope TEXT PRIMARY KEY, version TEXT NOT NULL);
CREATE TABLE scope_clears (scope TEXT PRIMARY KEY, version TEXT NOT NULL);
CREATE TABLE staged_changes (
 id TEXT PRIMARY KEY, device TEXT NOT NULL REFERENCES devices(id), digest TEXT,
 page_count INTEGER NOT NULL DEFAULT 0, byte_count INTEGER NOT NULL DEFAULT 0, change_count INTEGER NOT NULL DEFAULT 0,
 expires INTEGER NOT NULL DEFAULT (unixepoch()+86400)
);
CREATE INDEX staged_changes_device ON staged_changes(device);
CREATE TABLE staged_pages (stage TEXT NOT NULL REFERENCES staged_changes(id) ON DELETE CASCADE, ordinal INTEGER NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(stage,ordinal));
CREATE TABLE staged_records (stage TEXT NOT NULL REFERENCES staged_changes(id) ON DELETE CASCADE, key TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(stage,key));
CREATE TABLE staged_fences (stage TEXT NOT NULL REFERENCES staged_changes(id) ON DELETE CASCADE, key TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(stage,key));
CREATE TABLE staged_scope_fences (stage TEXT NOT NULL REFERENCES staged_changes(id) ON DELETE CASCADE, scope TEXT NOT NULL, body TEXT NOT NULL, PRIMARY KEY(stage,scope));
CREATE TABLE receipts (
 operation TEXT PRIMARY KEY, device TEXT NOT NULL REFERENCES devices(id), seq TEXT NOT NULL,
 digest TEXT NOT NULL, body TEXT NOT NULL, created INTEGER NOT NULL DEFAULT (unixepoch()), UNIQUE(device,seq)
);
CREATE TABLE commit_jobs (operation TEXT PRIMARY KEY,device TEXT NOT NULL UNIQUE REFERENCES devices(id),digest TEXT NOT NULL,body TEXT NOT NULL,stage TEXT NOT NULL,retry_after INTEGER NOT NULL DEFAULT 0);
CREATE TABLE commits (seq TEXT PRIMARY KEY, head TEXT NOT NULL, operation TEXT NOT NULL UNIQUE);
CREATE TABLE changes (
 seq TEXT NOT NULL REFERENCES commits(seq), ordinal INTEGER NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(seq,ordinal)
);
CREATE INDEX changes_cursor ON changes(length(seq),seq,ordinal);
PRAGMA user_version=8;
"#;
