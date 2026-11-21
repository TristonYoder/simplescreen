CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE users (
  subject TEXT PRIMARY KEY,
  display_name TEXT,
  email TEXT,
  last_seen_at TEXT NOT NULL
);

CREATE TABLE groups (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  oidc_bindings TEXT NOT NULL DEFAULT '[]',
  created_at TEXT NOT NULL
);

CREATE TABLE media (
  id TEXT PRIMARY KEY,
  group_id TEXT NOT NULL REFERENCES groups(id),
  kind TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  mime TEXT NOT NULL,
  title TEXT NOT NULL,
  width INTEGER,
  height INTEGER,
  duration_ms INTEGER,
  codec TEXT,
  hdr INTEGER,
  frame_rate REAL,
  source_filename TEXT,
  thumbnail_sha256 TEXT,
  size INTEGER NOT NULL,
  created_by TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX media_group ON media(group_id);
CREATE INDEX media_sha ON media(sha256);

CREATE TABLE playlists (
  id TEXT PRIMARY KEY,
  group_id TEXT NOT NULL REFERENCES groups(id),
  name TEXT NOT NULL,
  version INTEGER NOT NULL,
  loop INTEGER NOT NULL,
  defaults TEXT NOT NULL,
  items TEXT NOT NULL,
  beds TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  updated_by TEXT
);
CREATE INDEX playlists_group ON playlists(group_id);

CREATE TABLE playlist_media (
  playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  media_id TEXT NOT NULL REFERENCES media(id),
  PRIMARY KEY (playlist_id, media_id)
);
CREATE INDEX playlist_media_media ON playlist_media(media_id);

CREATE TABLE screens (
  id TEXT PRIMARY KEY,
  group_id TEXT NOT NULL REFERENCES groups(id),
  name TEXT NOT NULL,
  playlist_id TEXT REFERENCES playlists(id) ON DELETE SET NULL,
  token_hash TEXT NOT NULL UNIQUE,
  last_seen_at TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX screens_group ON screens(group_id);

CREATE TABLE pairings (
  id TEXT PRIMARY KEY,
  code TEXT NOT NULL,
  secret_hash TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  screen_id TEXT,
  device_token TEXT,
  token_delivered INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);
CREATE INDEX pairings_code ON pairings(code);
