use crate::ingest::{build_search_text, is_secret_markup_line, parse_file};
use crate::normalize::{now_rfc3339, sha256_file_hex};
use crate::search::{self, SearchQuery};
use crate::split::{refine_drafts, EntrySplitter};
use crate::types::*;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Store {
    pub path: PathBuf,
    conn: Connection,
    campaign_id: i64,
}

#[derive(Clone, Copy, Default)]
pub struct ImportOpts<'a> {
    pub splitter: Option<&'a dyn EntrySplitter>,
    pub reprocess: bool,
    /// 语义编码器：有则导入期计算子块向量（§7.2）；无则不写 embedding_chunks（§7.6）。
    pub embedder: Option<&'a crate::embed::Embedder>,
}

impl Store {
    pub fn create(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let len = fs::metadata(&path)?.len();
            if len == 0 {
                fs::remove_file(&path)?;
            } else {
                bail!("文件已存在: {}", path.display());
            }
        }
        let conn = Connection::open(&path)?;
        configure_new(&conn)?;
        conn.execute_batch(include_str!("schema.sql"))?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO schema_meta(key,value) VALUES ('app','table-canon'),('format','tcs')",
            [],
        )?;
        conn.execute(
            "INSERT INTO campaigns(name, created_at, updated_at) VALUES (?1,?2,?3)",
            params![name, now, now],
        )?;
        let campaign_id = conn.last_insert_rowid();
        seed_templates(&conn, campaign_id, &now)?;
        Ok(Self {
            path,
            conn,
            campaign_id,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, true)
    }

    /// 打开便携快照：不把 journal_mode 切成 WAL（Android / 验收用）。
    pub fn open_portable(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, false)
    }

    fn open_with(path: impl AsRef<Path>, wal: bool) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let app_id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        if app_id != 0 && app_id != APP_ID {
            bail!("不是席间索库文件 (application_id={app_id:#x})");
        }
        let ver: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if ver == 0 {
            conn.execute_batch(include_str!("schema.sql"))?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            conn.pragma_update(None, "application_id", APP_ID)?;
        } else if ver > SCHEMA_VERSION {
            bail!("库版本 {ver} 新于本程序 {SCHEMA_VERSION}，只读打开未实现，请升级软件");
        } else if ver < SCHEMA_VERSION {
            conn.execute_batch(include_str!("schema.sql"))?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        if wal {
            let _ = conn.pragma_update(None, "journal_mode", "WAL");
        }
        let campaign_id: i64 = conn
            .query_row("SELECT id FROM campaigns ORDER BY id LIMIT 1", [], |r| {
                r.get(0)
            })
            .context("库中没有战役")?;
        Ok(Self {
            path,
            conn,
            campaign_id,
        })
    }

    pub fn info(&self) -> Result<StoreInfo> {
        let name: String =
            self.conn
                .query_row("SELECT name FROM campaigns WHERE id=?1", [self.campaign_id], |r| {
                    r.get(0)
                })?;
        let chunk_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE campaign_id=?1",
            [self.campaign_id],
            |r| r.get(0),
        )?;
        let semantic_ready: bool = self
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM embedding_meta) > 0
                     AND (SELECT COUNT(*) FROM embedding_chunks WHERE campaign_id=?1) > 0",
                params![self.campaign_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .unwrap_or(false);
        Ok(StoreInfo {
            campaign_id: self.campaign_id,
            campaign_name: name,
            chunk_count,
            semantic_ready,
        })
    }

    pub fn close(self) -> Result<bool> {
        let (busy, log, checkpointed): (i64, i64, i64) = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .context("checkpoint 失败")?;
        if busy != 0 {
            bail!("checkpoint 未完成 busy={busy} log={log} checkpointed={checkpointed}，仍有连接占用");
        }
        Ok(true)
    }

    pub fn export_snapshot(&self, target: impl AsRef<Path>) -> Result<(u64, String)> {
        let target = target.as_ref();
        if target.exists() {
            fs::remove_file(target)?;
        }
        {
            let mut dst = Connection::open(target)?;
            let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)?;
            backup.run_to_completion(100, std::time::Duration::from_millis(5), None)?;
        }
        let dst = Connection::open(target)?;
        dst.pragma_update(None, "journal_mode", "DELETE")?;
        let mode: String = dst.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            bail!("快照 journal_mode 不是 delete，实际={mode}");
        }
        drop(dst);
        let bytes = fs::metadata(target)?.len();
        Ok((bytes, mode))
    }

    pub fn import_paths(&mut self, paths: &[PathBuf]) -> Result<ImportReport> {
        self.import_with(paths, ImportOpts::default())
    }

    pub fn import_with(&mut self, paths: &[PathBuf], opts: ImportOpts<'_>) -> Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        for p in paths {
            if p.is_dir() {
                for e in walkdir::WalkDir::new(p).into_iter().filter_map(|e| e.ok()) {
                    if e.file_type().is_file() {
                        let abs = e.path().to_path_buf();
                        let rel = store_rel_path(p, &abs);
                        files.push((abs, rel));
                    }
                }
            } else if p.is_file() {
                let rel = p
                    .file_name()
                    .map(|s| s.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|| p.to_string_lossy().replace('\\', "/"));
                files.push((p.clone(), rel));
            } else {
                report.files_fail += 1;
                report.errors.push(format!("不存在: {}", p.display()));
            }
        }
        for (abs, rel) in files {
            match classify_import(&abs) {
                ImportClass::SilentSkip => {
                    report.files_skip += 1;
                }
                ImportClass::Unsupported(msg) => {
                    report.files_skip += 1;
                    report.errors.push(format!("{}: {msg}", abs.display()));
                }
                ImportClass::Doc => match self.import_one(&abs, &rel, opts) {
                    Ok(ImportOne::Skip) => report.files_skip += 1,
                    Ok(ImportOne::Done {
                        chunks,
                        unmatched,
                        notes,
                    }) => {
                        report.files_ok += 1;
                        report.chunks += chunks;
                        report.unmatched_corrections += unmatched;
                        report.warnings.extend(notes);
                    }
                    Err(e) => {
                        report.files_fail += 1;
                        report.errors.push(format!("{}: {e}", abs.display()));
                    }
                },
            }
        }
        Ok(report)
    }

    fn import_one(&mut self, path: &Path, rel_path: &str, opts: ImportOpts<'_>) -> Result<ImportOne> {
        let bytes = fs::read(path)?;
        let file_hash = sha256_file_hex(&bytes);
        let file_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel_path.to_string());
        let file_type = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("txt")
            .to_ascii_lowercase();

        let mut existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, content_hash FROM source_documents WHERE campaign_id=?1 AND file_path=?2",
                params![self.campaign_id, rel_path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if existing.is_none() {
            let mut stmt = self.conn.prepare(
                "SELECT id, file_path FROM source_documents WHERE campaign_id=?1 AND content_hash=?2",
            )?;
            let hits: Vec<(i64, String)> = stmt
                .query_map(params![self.campaign_id, file_hash], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if hits.len() == 1 {
                let (id, _) = &hits[0];
                self.conn.execute(
                    "UPDATE source_documents SET file_path=?1, file_name=?2 WHERE id=?3",
                    params![rel_path, file_name, id],
                )?;
                existing = Some((*id, file_hash.clone()));
            }
        }

        if let Some((_, h)) = &existing {
            if h == &file_hash && !opts.reprocess {
                return Ok(ImportOne::Skip);
            }
        }

        let blocks = parse_file(path)?;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file_name.clone());
        let (mut drafts, mut notes) = refine_drafts(&blocks, &stem, opts.splitter);

        let tx = self.conn.unchecked_transaction()?;

        let doc_id = if let Some((id, _)) = existing {
            tx.execute(
                "UPDATE source_documents SET content_hash=?1, file_name=?2, file_type=?3, indexed_at=?4, file_path=?5 WHERE id=?6",
                params![file_hash, file_name, file_type, now_rfc3339(), rel_path, id],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO source_documents(campaign_id,file_path,file_name,file_type,content_hash,indexed_at)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    self.campaign_id,
                    rel_path,
                    file_name,
                    file_type,
                    file_hash,
                    now_rfc3339()
                ],
            )?;
            tx.last_insert_rowid()
        };

        let mut corrections: Vec<CorrectionRow> = Vec::new();
        let mut stmt = tx.prepare(
            "SELECT stable_key, alt_anchor, entity_type, visibility, aliases_json, tags_json
             FROM chunk_corrections WHERE source_document_id=?1",
        )?;
        let rows = stmt.query_map([doc_id], |r| {
            Ok(CorrectionRow {
                stable_key: r.get(0)?,
                alt_anchor: r.get::<_, Option<String>>(1)?,
                entity_type: r.get(2)?,
                visibility: r.get(3)?,
                aliases_json: r.get(4)?,
                tags_json: r.get(5)?,
            })
        })?;
        for row in rows {
            corrections.push(row?);
        }
        drop(stmt);

        let mut used = vec![false; corrections.len()];
        let mut draft_hit = vec![false; drafts.len()];
        for (di, d) in drafts.iter_mut().enumerate() {
            if let Some((i, _)) = corrections
                .iter()
                .enumerate()
                .find(|(i, c)| !used[*i] && c.stable_key == d.stable_key)
            {
                apply_correction(d, &corrections[i]);
                used[i] = true;
                draft_hit[di] = true;
            }
        }
        for (di, d) in drafts.iter_mut().enumerate() {
            if draft_hit[di] {
                continue;
            }
            if let Some((i, _)) = corrections.iter().enumerate().find(|(i, c)| {
                !used[*i] && c.alt_anchor.as_deref() == Some(&d.alt_anchor)
            }) {
                apply_correction(d, &corrections[i]);
                used[i] = true;
                draft_hit[di] = true;
            }
        }
        let unmatched = used.iter().filter(|u| !**u).count();

        tx.execute("DELETE FROM chunks WHERE source_document_id=?1", [doc_id])?;

        // 语义向量导入期计算（§7.2）：编码在事务外做（慢），写库在事务内（快）。
        // 单条失败只影响该条向量，不阻断导入。
        let mut vec_rows: Vec<(usize, i64, String, Vec<u8>)> = Vec::new();
        if let Some(e) = opts.embedder {
            let mut first_err: Option<String> = None;
            'enc: for (di, d) in drafts.iter().enumerate() {
                for (seq, sub) in crate::embed::split_subblocks(&d.body).into_iter().enumerate() {
                    match e.encode_doc(&sub) {
                        Ok(v) => vec_rows.push((di, seq as i64, sub.clone(), crate::embed::vec_to_blob(&v))),
                        Err(err) => {
                            first_err.get_or_insert_with(|| {
                                format!("「{}」语义向量计算失败，该条暂无向量：{err}", d.title)
                            });
                            vec_rows.retain(|r| r.0 != di);
                            continue 'enc;
                        }
                    }
                }
            }
            if let Some(msg) = first_err {
                notes.push(msg);
            }
        }

        let now = now_rfc3339();
        let n = drafts.len();
        let mut chunk_ids: Vec<i64> = Vec::with_capacity(n);
        for d in drafts {
            let aliases_json = serde_json::to_string(&d.aliases)?;
            let search_text = build_search_text(&d.title, &d.body, &d.aliases, &[]);
            tx.execute(
                "INSERT INTO chunks(
                    campaign_id, source_document_id, stable_key, ordinal, entity_type,
                    title, body, body_markdown, parent_path, visibility, aliases_json, tags_json,
                    search_text, source_rank, content_hash, created_at, updated_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'[]',?12,100,?13,?14,?14)",
                params![
                    self.campaign_id,
                    doc_id,
                    d.stable_key,
                    d.ordinal,
                    d.entity_type,
                    d.title,
                    d.body,
                    d.body,
                    d.parent_path,
                    d.visibility,
                    aliases_json,
                    search_text,
                    d.content_hash,
                    now
                ],
            )?;
            chunk_ids.push(tx.last_insert_rowid());
        }
        if !vec_rows.is_empty() {
            if ensure_embedding_meta(&tx, opts.embedder)? {
                notes.push("语义模型已切换，旧向量已清空，其余文档将按需补算".into());
            }
            for (di, seq, sub, blob) in vec_rows {
                tx.execute(
                    "INSERT INTO embedding_chunks(campaign_id, chunk_id, seq, body, embedding)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![self.campaign_id, chunk_ids[di], seq, sub, blob],
                )?;
            }
        }
        tx.commit()?;
        Ok(ImportOne::Done {
            chunks: n,
            unmatched,
            notes,
        })
    }

    pub fn search(&self, query: &str) -> Result<SearchResult> {
        self.search_with(query, None)
    }

    /// 带语义编码器的检索：`embedder=None` 或模型未就绪时自动降级纯词法（§7.3）。
    pub fn search_with(
        &self,
        query: &str,
        embedder: Option<&crate::embed::Embedder>,
    ) -> Result<SearchResult> {
        search::search(
            &self.conn,
            self.campaign_id,
            SearchQuery {
                text: query,
                embed: embedder,
            },
        )
    }

    /// 给缺向量的条目补算（§7.2：模型预热晚于导入时）。返回补算条目数与警告。
    pub fn backfill_embeddings(
        &self,
        e: &crate::embed::Embedder,
        progress: Option<&std::sync::Mutex<String>>,
    ) -> Result<(usize, Vec<String>)> {
        let mut notes = Vec::new();
        let tx = self.conn.unchecked_transaction()?;
        if ensure_embedding_meta(&tx, Some(e))? {
            notes.push("语义模型已切换，旧向量已清空并全量重算".into());
        }
        let rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT c.id, c.body FROM chunks c
                 WHERE c.campaign_id=?1
                   AND NOT EXISTS (SELECT 1 FROM embedding_chunks ec WHERE ec.chunk_id=c.id)",
            )?;
            let it = stmt.query_map(params![self.campaign_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            it.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let total = rows.len();
        for (k, (cid, body)) in rows.into_iter().enumerate() {
            let mut buf: Vec<(i64, String, Vec<u8>)> = Vec::new();
            let mut failed = false;
            for (seq, sub) in crate::embed::split_subblocks(&body).into_iter().enumerate() {
                match e.encode_doc(&sub) {
                    Ok(v) => buf.push((seq as i64, sub, crate::embed::vec_to_blob(&v))),
                    Err(err) => {
                        notes.push(format!("条目 {cid} 向量计算失败，已跳过：{err}"));
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                continue;
            }
            for (seq, sub, blob) in buf {
                tx.execute(
                    "INSERT INTO embedding_chunks(campaign_id, chunk_id, seq, body, embedding)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![self.campaign_id, cid, seq, sub, blob],
                )?;
            }
            if let Some(p) = progress {
                if let Ok(mut g) = p.lock() {
                    *g = format!("语义向量补算 {}/{total} 条…", k + 1);
                }
            }
        }
        tx.commit()?;
        Ok((total, notes))
    }

    pub fn get_chunk(&self, id: i64) -> Result<ChunkDetail> {
        let chunk = load_chunk(&self.conn, id)?;
        let prev_id = self
            .conn
            .query_row(
                "SELECT id FROM chunks WHERE source_document_id=?1 AND ordinal=?2",
                params![chunk.source_document_id, chunk.ordinal - 1],
                |r| r.get(0),
            )
            .optional()?;
        let next_id = self
            .conn
            .query_row(
                "SELECT id FROM chunks WHERE source_document_id=?1 AND ordinal=?2",
                params![chunk.source_document_id, chunk.ordinal + 1],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ChunkDetail {
            chunk,
            prev_id,
            next_id,
        })
    }

    pub fn copy_payload(&self, id: i64, template_key: &str) -> Result<String> {
        let chunk = load_chunk(&self.conn, id)?;
        let tmpl: String = self
            .conn
            .query_row(
                "SELECT body FROM copy_templates WHERE campaign_id=?1 AND key=?2",
                params![self.campaign_id, template_key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| default_template(template_key).to_string());
        let public_body = public_body(&chunk.body, chunk.visibility);
        Ok(tmpl
            .replace("{entity_type}", &chunk.entity_type)
            .replace("{title}", &chunk.title)
            .replace("{body}", &chunk.body)
            .replace("{public_body}", &public_body)
            .replace(
                "{source_path}",
                &format!("{} · {}", chunk.file_name, chunk.parent_path),
            ))
    }

    pub fn update_chunk_meta(&self, id: i64, patch: MetaPatch) -> Result<()> {
        let chunk = load_chunk(&self.conn, id)?;
        let now = now_rfc3339();
        let entity_type = patch.entity_type.clone().unwrap_or(chunk.entity_type.clone());
        let visibility = patch.visibility.unwrap_or(chunk.visibility);
        let aliases = patch.aliases.clone().unwrap_or(chunk.aliases.clone());
        let aliases_json = serde_json::to_string(&aliases)?;
        let search_text = build_search_text(&chunk.title, &chunk.body, &aliases, &[]);

        self.conn.execute(
            "INSERT INTO chunk_corrections(campaign_id, source_document_id, stable_key, alt_anchor, entity_type, visibility, aliases_json, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(source_document_id, stable_key) DO UPDATE SET
               entity_type=excluded.entity_type,
               visibility=excluded.visibility,
               aliases_json=excluded.aliases_json,
               alt_anchor=excluded.alt_anchor,
               updated_at=excluded.updated_at",
            params![
                self.campaign_id,
                chunk.source_document_id,
                chunk.stable_key,
                crate::normalize::alt_anchor(&chunk.parent_path, &chunk.body),
                entity_type,
                visibility,
                aliases_json,
                now
            ],
        )?;
        self.conn.execute(
            "UPDATE chunks SET entity_type=?1, visibility=?2, aliases_json=?3, search_text=?4, updated_at=?5 WHERE id=?6",
            params![entity_type, visibility, aliases_json, search_text, now, id],
        )?;
        Ok(())
    }

    pub fn upsert_synonym(&self, term: &str, canonical: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO synonyms(campaign_id, term, canonical, created_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(campaign_id, term) DO UPDATE SET canonical=excluded.canonical",
            params![self.campaign_id, term, canonical, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn list_synonyms(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT term, canonical FROM synonyms WHERE campaign_id=?1 ORDER BY term",
        )?;
        let rows = stmt.query_map([self.campaign_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn set_copy_template(&self, key: &str, body: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO copy_templates(campaign_id,key,body,updated_at) VALUES(?1,?2,?3,?4)
             ON CONFLICT(campaign_id,key) DO UPDATE SET body=excluded.body, updated_at=excluded.updated_at",
            params![self.campaign_id, key, body, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn journal_mode(&self) -> Result<String> {
        Ok(self.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?)
    }

    pub fn source_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path FROM source_documents WHERE campaign_id=?1 ORDER BY file_path",
        )?;
        let rows = stmt.query_map([self.campaign_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

/// 校验/写入 embedding_meta（§文档：两端模型强校验）。
/// 返回 true 表示模型相对库中记录发生了切换，且旧向量已被清空。
fn ensure_embedding_meta(
    conn: &rusqlite::Connection,
    e: Option<&crate::embed::Embedder>,
) -> Result<bool> {
    let Some(e) = e else {
        return Ok(false);
    };
    let existing: Option<String> = conn
        .query_row("SELECT model_id FROM embedding_meta WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO embedding_meta(id, model_id, model_version, dim, quant, runtime, pooling, query_prefix)
                 VALUES(1,?1,'1',?2,?3,'onnx','cls',?4)",
                params![
                    e.model_id(),
                    crate::embed::EMBED_DIM as i64,
                    crate::embed::META_QUANT,
                    crate::embed::QUERY_PREFIX
                ],
            )?;
            Ok(false)
        }
        Some(id) if id == e.model_id() => Ok(false),
        Some(old) => {
            conn.execute("DELETE FROM embedding_chunks", [])?;
            conn.execute(
                "UPDATE embedding_meta SET model_id=?1, model_version='1', dim=?2, quant=?3, runtime='onnx', pooling='cls', query_prefix=?4 WHERE id=1",
                params![
                    e.model_id(),
                    crate::embed::EMBED_DIM as i64,
                    crate::embed::META_QUANT,
                    crate::embed::QUERY_PREFIX
                ],
            )?;
            let _ = old;
            Ok(true)
        }
    }
}

enum ImportOne {
    Skip,
    Done {
        chunks: usize,
        unmatched: usize,
        /// 拆条阶段的非致命警告（失败回退、切点贴不回等）。
        notes: Vec<String>,
    },
}

enum ImportClass {
    Doc,
    SilentSkip,
    Unsupported(&'static str),
}

struct CorrectionRow {
    stable_key: String,
    alt_anchor: Option<String>,
    entity_type: Option<String>,
    visibility: Option<i64>,
    aliases_json: Option<String>,
    #[allow(dead_code)]
    tags_json: Option<String>,
}

fn classify_import(path: &Path) -> ImportClass {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "txt" | "html" | "htm" | "rtf" | "docx" => ImportClass::Doc,
        "pdf" => ImportClass::Unsupported("PDF 文本层抽取未纳入本 demo"),
        "doc" => ImportClass::Unsupported("不支持 .doc，请另存为 .docx"),
        _ => ImportClass::SilentSkip,
    }
}

fn store_rel_path(root: &Path, file: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => file
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string_lossy().replace('\\', "/")),
    }
}

fn apply_correction(d: &mut crate::types::DraftChunk, c: &CorrectionRow) {
    if let Some(t) = &c.entity_type {
        d.entity_type = t.clone();
    }
    if let Some(v) = c.visibility {
        d.visibility = v;
    }
    if let Some(aj) = &c.aliases_json {
        if let Ok(extra) = serde_json::from_str::<Vec<String>>(aj) {
            for a in extra {
                if !d.aliases.contains(&a) {
                    d.aliases.push(a);
                }
            }
        }
    }
}

fn configure_new(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "application_id", APP_ID)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

fn seed_templates(conn: &Connection, campaign_id: i64, now: &str) -> Result<()> {
    for (k, b) in [
        (
            "player",
            "{entity_type}　{title}\n{public_body}\n—— {source_path}",
        ),
        ("full", "{title}\n{body}\n来源：{source_path}"),
        ("source", "{source_path}"),
    ] {
        conn.execute(
            "INSERT INTO copy_templates(campaign_id,key,body,updated_at) VALUES(?1,?2,?3,?4)",
            params![campaign_id, k, b, now],
        )?;
    }
    Ok(())
}

fn default_template(key: &str) -> &'static str {
    match key {
        "full" => "{title}\n{body}\n来源：{source_path}",
        "source" => "{source_path}",
        _ => "{entity_type}　{title}\n{public_body}\n—— {source_path}",
    }
}

pub fn load_chunk(conn: &Connection, id: i64) -> Result<Chunk> {
    conn.query_row(
        "SELECT c.id, c.source_document_id, c.stable_key, c.ordinal, c.entity_type, c.title, c.body,
                c.parent_path, c.visibility, c.aliases_json, c.source_rank, d.file_name
         FROM chunks c JOIN source_documents d ON d.id=c.source_document_id
         WHERE c.id=?1",
        [id],
        |r| {
            let aliases_json: String = r.get(9)?;
            let aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
            Ok(Chunk {
                id: r.get(0)?,
                source_document_id: r.get(1)?,
                stable_key: r.get(2)?,
                ordinal: r.get(3)?,
                entity_type: r.get(4)?,
                title: r.get(5)?,
                body: r.get(6)?,
                parent_path: r.get(7)?,
                visibility: r.get(8)?,
                aliases,
                source_rank: r.get(10)?,
                file_name: r.get(11)?,
            })
        },
    )
    .context("chunk 不存在")
}

pub fn public_body(body: &str, visibility: i64) -> String {
    let mut lines = Vec::new();
    let mut stripped = false;
    for line in body.lines() {
        if is_secret_markup_line(line) {
            stripped = true;
            continue;
        }
        lines.push(line);
    }
    let s = lines.join("\n");
    let hidden_bit = visibility & (VIS_SECRET | VIS_DM_ONLY | VIS_HIDDEN) != 0;
    if s.trim().is_empty() && (hidden_bit || stripped) {
        "（该条目对玩家隐藏）".into()
    } else if s.trim().is_empty() {
        body.to_string()
    } else {
        s
    }
}

pub fn load_synonyms(conn: &Connection, campaign_id: i64) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT term, canonical FROM synonyms WHERE campaign_id=?1")
    {
        if let Ok(rows) = stmt.query_map([campaign_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for r in rows.flatten() {
                map.insert(r.0, r.1);
            }
        }
    }
    map
}
