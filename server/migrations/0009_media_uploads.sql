CREATE TABLE media_uploads (
  id TEXT PRIMARY KEY,
  s3_upload_id TEXT NOT NULL,
  library_id TEXT NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  size INTEGER NOT NULL,
  part_size INTEGER NOT NULL,
  part_count INTEGER NOT NULL,
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE media_upload_parts (
  upload_id TEXT NOT NULL REFERENCES media_uploads(id) ON DELETE CASCADE,
  number INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  PRIMARY KEY (upload_id, number)
);
