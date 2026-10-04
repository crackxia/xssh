-- Download counts per release file and day.
CREATE TABLE IF NOT EXISTS downloads (
  version TEXT NOT NULL,
  target  TEXT NOT NULL,
  day     TEXT NOT NULL,
  count   INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (version, target, day)
);
