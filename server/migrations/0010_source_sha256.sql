ALTER TABLE media ADD COLUMN source_sha256 TEXT;
UPDATE media SET source_sha256 = sha256;
CREATE INDEX media_source_sha256 ON media(source_sha256);
