ALTER TABLE groups RENAME TO teams;
ALTER TABLE media RENAME COLUMN group_id TO team_id;
ALTER TABLE playlists RENAME COLUMN group_id TO team_id;
ALTER TABLE screens RENAME COLUMN group_id TO team_id;
ALTER TABLE schedule_entries RENAME COLUMN group_id TO team_id;
ALTER TABLE floor_plans RENAME COLUMN group_id TO team_id;

-- One org zone replaces per-team zones. Copy it only when every set zone agrees.
INSERT INTO settings (key, value)
SELECT 'org_timezone', tz FROM (
  SELECT MIN(timezone) AS tz, COUNT(DISTINCT timezone) AS n
  FROM teams
  WHERE timezone IS NOT NULL AND timezone != ''
)
WHERE n = 1
ON CONFLICT(key) DO NOTHING;

ALTER TABLE teams DROP COLUMN timezone;
