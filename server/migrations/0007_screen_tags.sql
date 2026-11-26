-- Screens, nodes, floor plans and tags move to the org. Teams keep content and gain reach through tags.
-- Screens, nodes, assignments, floor plans, pairings and schedule entries are dev data and are dropped:
-- devices re-pair.
DROP TABLE assignments;
DROP TABLE screens;
DROP TABLE nodes;
DROP TABLE floor_plans;
DELETE FROM pairings;
DELETE FROM schedule_entries;

ALTER TABLE teams ADD COLUMN screen_access TEXT NOT NULL DEFAULT 'tags';
ALTER TABLE users ADD COLUMN timezone TEXT;

CREATE TABLE tags (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL COLLATE NOCASE UNIQUE,
  color TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE team_tags (
  team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (team_id, tag_id)
);
CREATE INDEX team_tags_tag ON team_tags(tag_id);

CREATE TABLE playlist_folders (
  id TEXT PRIMARY KEY,
  team_id TEXT NOT NULL REFERENCES teams(id),
  parent_id TEXT REFERENCES playlist_folders(id),
  name TEXT NOT NULL,
  position INTEGER NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX playlist_folders_team ON playlist_folders(team_id);

ALTER TABLE playlists ADD COLUMN folder_id TEXT REFERENCES playlist_folders(id) ON DELETE SET NULL;

CREATE TABLE playlist_tags (
  playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (playlist_id, tag_id)
);
CREATE INDEX playlist_tags_tag ON playlist_tags(tag_id);

CREATE TABLE floor_plans (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  image_sha256 TEXT,
  image_width INTEGER,
  image_height INTEGER,
  position INTEGER NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE nodes (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  model TEXT,
  os_version TEXT,
  app_version TEXT,
  outputs TEXT NOT NULL DEFAULT '[]',
  single INTEGER NOT NULL DEFAULT 0,
  mode TEXT NOT NULL DEFAULT 'player',
  assignments_version INTEGER NOT NULL DEFAULT 0,
  applied_assignments_version INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  last_seen_at TEXT
);

-- node_id is where the screen's program outputs are (null = unplaced).
CREATE TABLE screens (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  fallback_playlist_id TEXT REFERENCES playlists(id) ON DELETE SET NULL,
  timezone TEXT,
  last_seen_at TEXT,
  created_at TEXT NOT NULL,
  override_set INTEGER NOT NULL DEFAULT 0,
  override_playlist_id TEXT,
  override_until TEXT,
  override_set_at TEXT,
  override_set_by TEXT,
  override_set_by_name TEXT,
  override_team_id TEXT,
  placement_plan_id TEXT REFERENCES floor_plans(id) ON DELETE SET NULL,
  placement_x REAL,
  placement_y REAL,
  node_id TEXT REFERENCES nodes(id)
);
CREATE INDEX screens_node ON screens(node_id);

CREATE TABLE screen_tags (
  screen_id TEXT NOT NULL REFERENCES screens(id) ON DELETE CASCADE,
  tag_id TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  PRIMARY KEY (screen_id, tag_id)
);
CREATE INDEX screen_tags_tag ON screen_tags(tag_id);

CREATE TABLE assignments (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  output_key TEXT NOT NULL,
  job TEXT NOT NULL,
  screen_id TEXT REFERENCES screens(id) ON DELETE SET NULL,
  settings TEXT NOT NULL DEFAULT '{}',
  updated_by TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (node_id, output_key)
);
CREATE INDEX assignments_screen ON assignments(screen_id);

ALTER TABLE schedule_entries ADD COLUMN tag_ids TEXT NOT NULL DEFAULT '[]';
ALTER TABLE schedule_entries ADD COLUMN created_by TEXT;
ALTER TABLE schedule_entries ADD COLUMN needs_resolution INTEGER NOT NULL DEFAULT 0;

CREATE TABLE screen_events (
  id TEXT PRIMARY KEY,
  screen_id TEXT NOT NULL REFERENCES screens(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  user_sub TEXT,
  user_name TEXT,
  team_id TEXT,
  detail TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL
);
CREATE INDEX screen_events_screen ON screen_events(screen_id, created_at);

-- user_sub null = addressed to every admin.
CREATE TABLE notices (
  id TEXT PRIMARY KEY,
  user_sub TEXT,
  kind TEXT NOT NULL,
  screen_id TEXT,
  entry_id TEXT,
  detail TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL,
  resolved_at TEXT,
  dismissed_at TEXT
);
CREATE INDEX notices_user ON notices(user_sub);
