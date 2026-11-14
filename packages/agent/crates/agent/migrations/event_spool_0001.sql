-- Version 1 of the event spool, which every file starts from. A later layout is a migration
-- from it, in its own event_spool_NNNN.sql, that names the version it reaches.

CREATE TABLE server_streams (
  allocation TEXT PRIMARY KEY,
  received INTEGER NOT NULL DEFAULT -1,
  updatedAt INTEGER NOT NULL
);

CREATE TABLE server_events (
  id INTEGER PRIMARY KEY,
  allocation TEXT NOT NULL REFERENCES server_streams(allocation),
  seq INTEGER NOT NULL CHECK(seq >= 0),
  frame TEXT NOT NULL,
  UNIQUE(allocation, seq)
);

CREATE TABLE exits (
  allocation TEXT PRIMARY KEY,
  slot TEXT NOT NULL,
  reason TEXT NOT NULL,
  exitCode INTEGER,
  endedAt INTEGER NOT NULL,
  detail TEXT
);

CREATE TABLE allocations (
  allocation TEXT PRIMARY KEY,
  launchedAt INTEGER NOT NULL
);
