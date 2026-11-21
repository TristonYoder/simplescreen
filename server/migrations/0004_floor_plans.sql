CREATE TABLE floor_plans (
  id TEXT PRIMARY KEY,
  group_id TEXT NOT NULL REFERENCES groups(id),
  name TEXT NOT NULL,
  image_sha256 TEXT,
  image_width INTEGER,
  image_height INTEGER,
  position INTEGER NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX floor_plans_group ON floor_plans(group_id);
ALTER TABLE screens ADD COLUMN placement_plan_id TEXT REFERENCES floor_plans(id) ON DELETE SET NULL;
ALTER TABLE screens ADD COLUMN placement_x REAL;
ALTER TABLE screens ADD COLUMN placement_y REAL;
