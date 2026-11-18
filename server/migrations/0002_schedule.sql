ALTER TABLE groups ADD COLUMN timezone TEXT;

ALTER TABLE screens ADD COLUMN override_set INTEGER NOT NULL DEFAULT 0;
ALTER TABLE screens ADD COLUMN override_playlist_id TEXT;
ALTER TABLE screens ADD COLUMN override_until TEXT;
ALTER TABLE screens ADD COLUMN override_set_at TEXT;

CREATE TABLE schedule_entries (
  id TEXT PRIMARY KEY,
  group_id TEXT NOT NULL REFERENCES groups(id),
  playlist_id TEXT REFERENCES playlists(id) ON DELETE CASCADE,
  title TEXT,
  screen_ids TEXT NOT NULL DEFAULT '[]',
  kind TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  days TEXT NOT NULL DEFAULT '[]',
  start_time TEXT,
  end_time TEXT,
  starts_on TEXT,
  ends_on TEXT,
  starts_at TEXT,
  ends_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX schedule_entries_group ON schedule_entries(group_id);
