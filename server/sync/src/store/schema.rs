pub const SCHEMA: &str = r#"
CREATE TABLE library (singleton INTEGER PRIMARY KEY CHECK(singleton=1), head TEXT NOT NULL);
CREATE TABLE devices (
 id TEXT PRIMARY KEY, verifier TEXT NOT NULL UNIQUE, revoked INTEGER NOT NULL DEFAULT 0,
 watermark TEXT NOT NULL DEFAULT '0', ack TEXT NOT NULL DEFAULT '0'
);
CREATE TABLE objects (hash TEXT PRIMARY KEY, size INTEGER NOT NULL CHECK(size>=0));
CREATE TABLE records (key TEXT PRIMARY KEY, version TEXT NOT NULL);
CREATE TABLE staged_changes (
 id TEXT PRIMARY KEY, device TEXT NOT NULL REFERENCES devices(id), digest TEXT NOT NULL, body TEXT NOT NULL
);
CREATE TABLE receipts (
 operation TEXT PRIMARY KEY, device TEXT NOT NULL REFERENCES devices(id), seq TEXT NOT NULL,
 digest TEXT NOT NULL, body TEXT NOT NULL, UNIQUE(device,seq)
);
CREATE TABLE commits (seq TEXT PRIMARY KEY, head TEXT NOT NULL, operation TEXT NOT NULL UNIQUE);
CREATE TABLE changes (
 seq TEXT NOT NULL REFERENCES commits(seq), ordinal INTEGER NOT NULL, body TEXT NOT NULL,
 PRIMARY KEY(seq,ordinal)
);
CREATE INDEX changes_cursor ON changes(length(seq),seq,ordinal);
PRAGMA user_version=1;
"#;
