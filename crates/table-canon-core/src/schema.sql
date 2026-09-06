PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS schema_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS campaigns (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS source_documents (
  id            INTEGER PRIMARY KEY,
  campaign_id   INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  file_path     TEXT NOT NULL,
  file_name     TEXT NOT NULL,
  file_type     TEXT NOT NULL,
  content_hash  TEXT NOT NULL,
  source_rank   INTEGER NOT NULL DEFAULT 100,
  indexed_at    TEXT,
  UNIQUE(campaign_id, file_path)
);

CREATE TABLE IF NOT EXISTS chunks (
  id                  INTEGER PRIMARY KEY,
  campaign_id         INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  source_document_id  INTEGER NOT NULL REFERENCES source_documents(id) ON DELETE CASCADE,
  stable_key          TEXT NOT NULL,
  ordinal             INTEGER NOT NULL,
  entity_type         TEXT NOT NULL DEFAULT 'other',
  title               TEXT NOT NULL,
  body                TEXT NOT NULL,
  body_markdown       TEXT,
  parent_path         TEXT,
  visibility          INTEGER NOT NULL DEFAULT 1,
  aliases_json        TEXT NOT NULL DEFAULT '[]',
  tags_json           TEXT NOT NULL DEFAULT '[]',
  search_text         TEXT NOT NULL,
  source_rank         INTEGER NOT NULL DEFAULT 100,
  content_hash        TEXT,
  created_at          TEXT NOT NULL,
  updated_at          TEXT NOT NULL,
  UNIQUE(source_document_id, ordinal)
);

CREATE INDEX IF NOT EXISTS idx_chunks_campaign ON chunks(campaign_id);
CREATE INDEX IF NOT EXISTS idx_chunks_source   ON chunks(source_document_id);
CREATE INDEX IF NOT EXISTS idx_chunks_type     ON chunks(campaign_id, entity_type);
CREATE INDEX IF NOT EXISTS idx_chunks_stable   ON chunks(source_document_id, stable_key);

CREATE TABLE IF NOT EXISTS chunk_corrections (
  id                  INTEGER PRIMARY KEY,
  campaign_id         INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  source_document_id  INTEGER NOT NULL REFERENCES source_documents(id) ON DELETE CASCADE,
  stable_key          TEXT NOT NULL,
  alt_anchor          TEXT,
  entity_type         TEXT,
  visibility          INTEGER,
  aliases_json        TEXT,
  tags_json           TEXT,
  updated_at          TEXT NOT NULL,
  UNIQUE(source_document_id, stable_key)
);

CREATE INDEX IF NOT EXISTS idx_corrections_anchor
  ON chunk_corrections(source_document_id, alt_anchor);

CREATE TABLE IF NOT EXISTS embedding_chunks (
  id           INTEGER PRIMARY KEY,
  campaign_id  INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  chunk_id     INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  body         TEXT NOT NULL,
  embedding    BLOB NOT NULL,
  UNIQUE(chunk_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_embedding_chunks_chunk ON embedding_chunks(chunk_id);
CREATE INDEX IF NOT EXISTS idx_embedding_chunks_camp  ON embedding_chunks(campaign_id);

CREATE TABLE IF NOT EXISTS synonyms (
  id              INTEGER PRIMARY KEY,
  campaign_id     INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  term            TEXT NOT NULL,
  canonical       TEXT NOT NULL,
  target_chunk_id INTEGER REFERENCES chunks(id) ON DELETE SET NULL,
  created_at      TEXT NOT NULL,
  UNIQUE(campaign_id, term)
);

CREATE TABLE IF NOT EXISTS embedding_meta (
  id             INTEGER PRIMARY KEY CHECK (id = 1),
  model_id       TEXT NOT NULL,
  model_version  TEXT NOT NULL,
  dim            INTEGER NOT NULL,
  quant          TEXT NOT NULL,
  runtime        TEXT NOT NULL DEFAULT 'onnx',
  pooling        TEXT NOT NULL DEFAULT 'cls',
  query_prefix   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS query_log (
  id            INTEGER PRIMARY KEY,
  campaign_id   INTEGER NOT NULL,
  query         TEXT NOT NULL,
  latency_ms    INTEGER NOT NULL,
  used_semantic INTEGER NOT NULL,
  created_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS copy_templates (
  id           INTEGER PRIMARY KEY,
  campaign_id  INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  key          TEXT NOT NULL,
  body         TEXT NOT NULL,
  updated_at   TEXT NOT NULL,
  UNIQUE(campaign_id, key)
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(
  title,
  search_text,
  content='chunks',
  content_rowid='id',
  tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunk_fts(rowid, title, search_text)
  VALUES (new.id, new.title, new.search_text);
END;

CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunk_fts(chunk_fts, rowid, title, search_text)
  VALUES ('delete', old.id, old.title, old.search_text);
END;

CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
  INSERT INTO chunk_fts(chunk_fts, rowid, title, search_text)
  VALUES ('delete', old.id, old.title, old.search_text);
  INSERT INTO chunk_fts(rowid, title, search_text)
  VALUES (new.id, new.title, new.search_text);
END;
