CREATE TABLE endpoints (
  uuid TEXT PRIMARY KEY NOT NULL,
  envelope TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX endpoints_updated_at ON endpoints(updated_at);
