CREATE TABLE IF NOT EXISTS tickets (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  source     TEXT NOT NULL,
  contact    TEXT NOT NULL,
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
