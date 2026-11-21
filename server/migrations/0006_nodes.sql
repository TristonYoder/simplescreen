-- Nodes own the device token. A node reports outputs; each output is assigned a job, and a screen
-- is played by one output of one node. Pre-node screens and pairings are dev data and are dropped:
-- devices re-pair.
DELETE FROM pairings;
DROP TABLE screens;

CREATE TABLE nodes (
  id TEXT PRIMARY KEY,
  team_id TEXT NOT NULL REFERENCES teams(id),
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
CREATE INDEX nodes_team ON nodes(team_id);

CREATE TABLE screens (
  id TEXT PRIMARY KEY,
  team_id TEXT NOT NULL REFERENCES teams(id),
  name TEXT NOT NULL,
  playlist_id TEXT REFERENCES playlists(id) ON DELETE SET NULL,
  last_seen_at TEXT,
  created_at TEXT NOT NULL,
  override_set INTEGER NOT NULL DEFAULT 0,
  override_playlist_id TEXT,
  override_until TEXT,
  override_set_at TEXT,
  placement_plan_id TEXT REFERENCES floor_plans(id) ON DELETE SET NULL,
  placement_x REAL,
  placement_y REAL,
  node_id TEXT REFERENCES nodes(id),
  output_key TEXT
);
CREATE INDEX screens_team ON screens(team_id);
CREATE UNIQUE INDEX screens_node_output ON screens(node_id, output_key) WHERE node_id IS NOT NULL;

ALTER TABLE pairings ADD COLUMN kind TEXT NOT NULL DEFAULT 'single';
ALTER TABLE pairings ADD COLUMN mode TEXT NOT NULL DEFAULT 'player';
ALTER TABLE pairings ADD COLUMN node_id TEXT;

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
