-- Media moves into folders of org-owned libraries. Existing media is dev data and is dropped,
-- along with the playlist items and bed tracks that used it.
DELETE FROM playlist_media;
UPDATE playlists SET items = '[]', beds = '[]', version = version + 1;
DROP TABLE playlist_media;
DROP TABLE media;

CREATE TABLE libraries (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  path TEXT,
  s3 TEXT,
  s3_secret TEXT,
  quota_bytes INTEGER,
  position INTEGER NOT NULL,
  created_at TEXT NOT NULL
);
INSERT INTO libraries (id, name, kind, position, created_at)
  VALUES ('managed', 'Library', 'managed', 0, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

CREATE TABLE media_folders (
  id TEXT PRIMARY KEY,
  library_id TEXT NOT NULL REFERENCES libraries(id),
  parent_id TEXT REFERENCES media_folders(id),
  name TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX media_folders_parent ON media_folders(parent_id);
CREATE UNIQUE INDEX media_folders_name ON media_folders(library_id, COALESCE(parent_id, ''), name COLLATE NOCASE);

CREATE TABLE media_folder_tags (
  folder_id TEXT NOT NULL REFERENCES media_folders(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (folder_id, tag_id)
);

CREATE TABLE media_folder_grants (
  folder_id TEXT NOT NULL REFERENCES media_folders(id) ON DELETE CASCADE,
  team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  PRIMARY KEY (folder_id, team_id)
);

-- folder_id is null once the file is missing; path keeps where it was.
CREATE TABLE media (
  id TEXT PRIMARY KEY,
  library_id TEXT NOT NULL REFERENCES libraries(id),
  folder_id TEXT REFERENCES media_folders(id),
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  kind TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  mime TEXT NOT NULL,
  width INTEGER,
  height INTEGER,
  duration_ms INTEGER,
  codec TEXT,
  hdr INTEGER,
  frame_rate REAL,
  source_filename TEXT,
  thumbnail_sha256 TEXT,
  size INTEGER NOT NULL,
  missing_at TEXT,
  created_by TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX media_sha ON media(sha256);
CREATE INDEX media_library ON media(library_id);
CREATE UNIQUE INDEX media_name ON media(folder_id, name COLLATE NOCASE) WHERE folder_id IS NOT NULL;

CREATE TABLE playlist_media (
  playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  media_id TEXT NOT NULL REFERENCES media(id),
  PRIMARY KEY (playlist_id, media_id)
);
CREATE INDEX playlist_media_media ON playlist_media(media_id);

ALTER TABLE notices ADD COLUMN media_id TEXT;
