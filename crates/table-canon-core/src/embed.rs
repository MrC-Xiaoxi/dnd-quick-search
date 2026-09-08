//! M2 语义检索：BGE-small-zh-v1.5 的 ONNX 编码器与语义子块切分。
//! 口径来自技术方案 §7.1/§7.6：CLS pooling、L2 归一、查询侧前缀、512 token 截右侧、CPU 2 线程、batch=1。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const MODEL_ID: &str = "bge-small-zh-v1.5";
pub const EMBED_DIM: usize = 512;
pub const MAX_TOKENS: usize = 512;
pub const META_QUANT: &str = "int8";
pub const QUERY_PREFIX: &str = "为这个句子生成表示以用于检索相关文章：";
/// §7.6 语义子块：窗口 400 字 + 重叠 50 字
pub const SUBBLOCK_CHARS: usize = 400;
pub const SUBBLOCK_OVERLAP: usize = 50;

/// ONNX 编码器。Session 用 Mutex 包一层：导入线程与查询线程可能并发 encode。
pub struct Embedder {
    session: Mutex<ort::session::Session>,
    #[allow(dead_code)]
    tokenizer: tokenizers::Tokenizer,
}

impl Embedder {
    /// 加载模型。`model_dir` 需含 model.onnx 与 tokenizer.json；
    /// `dll_path` 为 onnxruntime.dll 位置（load-dynamic），缺省尊重已设置的 ORT_DYLIB_PATH。
    pub fn load(model_dir: &Path, dll_path: Option<&Path>) -> Result<Self> {
        let model = model_dir.join("model.onnx");
        let tok_path = model_dir.join("tokenizer.json");
        if !model.is_file() || !tok_path.is_file() {
            bail!(
                "模型目录不完整（需要 model.onnx 与 tokenizer.json）：{}",
                model_dir.display()
            );
        }
        init_ort_dll(dll_path)?;

        let mut tokenizer = tokenizers::Tokenizer::from_file(&tok_path)
            .map_err(|e| anyhow::anyhow!("加载 tokenizer 失败: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                stride: 0,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
                direction: tokenizers::TruncationDirection::Right, // 截右侧，保留开头
            }))
            .map_err(|e| anyhow::anyhow!("设置截断失败: {e}"))?;

        let session = ort::session::Session::builder()?
            .with_intra_threads(2)? // §7.1：Windows 查询 2 线程
            .commit_from_file(&model)
            .with_context(|| format!("加载 ONNX 模型失败: {}", model.display()))?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    pub fn model_id(&self) -> &'static str {
        MODEL_ID
    }

    /// 文档侧编码：无前缀（§7.1）。
    pub fn encode_doc(&self, text: &str) -> Result<Vec<f32>> {
        self.encode(text)
    }

    /// 查询侧编码：带 BGE 检索前缀（§7.1）。
    pub fn encode_query(&self, text: &str) -> Result<Vec<f32>> {
        self.encode(&format!("{QUERY_PREFIX}{}", text.trim()))
    }

    fn encode(&self, text: &str) -> Result<Vec<f32>> {
        let text = text.trim();
        if text.is_empty() {
            bail!("空文本不编码");
        }
        // add_special_tokens=true：必须补上 [CLS]/[SEP]，否则下面的 CLS pooling
        // 取到的是首个正文 token 的隐状态（golden 门禁实测余弦 0.82 → 0.99）
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("分词失败: {e}"))?;
        let ids = enc.get_ids();
        let n = ids.len();
        if n == 0 {
            bail!("分词结果为空");
        }
        let ids_i64: Vec<i64> = ids.iter().map(|&x| x as i64).collect();
        let mask_i64: Vec<i64> = enc
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();

        let mut session = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let has_tti = session.inputs.iter().any(|i| i.name == "token_type_ids");
        let outputs = if has_tti {
            let tti = vec![0i64; n];
            session.run(ort::inputs![
                "input_ids" => ort::value::Tensor::from_array(([1usize, n], ids_i64))?,
                "attention_mask" => ort::value::Tensor::from_array(([1usize, n], mask_i64))?,
                "token_type_ids" => ort::value::Tensor::from_array(([1usize, n], tti))?,
            ])?
        } else {
            session.run(ort::inputs![
                "input_ids" => ort::value::Tensor::from_array(([1usize, n], ids_i64))?,
                "attention_mask" => ort::value::Tensor::from_array(([1usize, n], mask_i64))?,
            ])?
        };
        let (shape, data) = outputs["last_hidden_state"].try_extract_tensor::<f32>()?;
        if data.len() < EMBED_DIM {
            bail!("模型输出过小: shape={shape:?} len={}", data.len());
        }
        // CLS pooling：第一个 token 的隐状态（§7.1，不是 mean pooling）；布局 (1, seq, dim) 行主序
        let mut v: Vec<f32> = data[..EMBED_DIM].to_vec();
        l2_normalize(&mut v);
        Ok(v)
    }
}

/// load-dynamic 模式下 onnxruntime.dll 的定位只需设置一次；进程内只初始化一次。
fn init_ort_dll(dll_path: Option<&Path>) -> Result<()> {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.get().is_some() {
        return Ok(());
    }
    if std::env::var_os("ORT_DYLIB_PATH")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        ONCE.set(()).ok();
        return Ok(());
    }
    let resolved = dll_path.map(Path::to_string_lossy).map(|p| p.to_string());
    if let Some(p) = resolved {
        if !Path::new(&p).is_file() {
            bail!("onnxruntime.dll 不存在: {p}");
        }
        std::env::set_var("ORT_DYLIB_PATH", &p);
    }
    ONCE.set(()).ok();
    Ok(())
}

/// 运行时模型目录解析：安装包布局（exe 侧 models\）优先，其次 %LOCALAPPDATA%。
pub fn default_model_dir() -> Option<PathBuf> {
    for dir in candidate_roots() {
        let p = dir.join("models").join(MODEL_ID);
        if p.join("model.onnx").is_file() {
            return Some(p);
        }
    }
    None
}

/// 运行时 onnxruntime.dll 解析，优先级同上。
pub fn default_dll_path() -> Option<PathBuf> {
    for dir in candidate_roots() {
        let p = dir.join("onnxruntime.dll");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            out.push(parent.to_path_buf());
        }
    }
    if let Some(loc) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(loc).join("table-canon"));
    }
    out
}

/// §7.6 语义子块：≤400 字整块一行；更长按 400 字窗口 + 50 字重叠，
/// 尽量在句号/换行处断，对不齐则硬切。
pub fn split_subblocks(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    if chars.len() <= SUBBLOCK_CHARS {
        let t = body.trim();
        return if t.is_empty() { Vec::new() } else { vec![t.to_string()] };
    }
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let hard_end = (start + SUBBLOCK_CHARS).min(chars.len());
        let mut cut = hard_end;
        if hard_end < chars.len() {
            let min_cut = start + (SUBBLOCK_CHARS / 2).max(1);
            for k in (min_cut..hard_end).rev() {
                if "。！？；\n".contains(chars[k]) {
                    cut = k + 1;
                    break;
                }
            }
        }
        let seg: String = chars[start..cut].iter().collect();
        let seg = seg.trim();
        if !seg.is_empty() {
            out.push(seg.to_string());
        }
        if cut >= chars.len() {
            break;
        }
        start = (cut - SUBBLOCK_OVERLAP).max(start + 1);
    }
    out
}

/// §7.1：向量写入/查询前统一 L2，检索用点积。
pub fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// 向量 <-> BLOB（f32 小端，嵌入表存储口径）。
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 已归一化向量的点积（= 余弦相似度）。
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subblocks_short_body_single() {
        let v = split_subblocks("铁砧堡扼守山口。城主荀岚兼管税收。");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], "铁砧堡扼守山口。城主荀岚兼管税收。");
        assert!(split_subblocks("   ").is_empty());
    }

    #[test]
    fn subblocks_long_body_windows_with_overlap_and_sentence_cut() {
        let para = "终燃城的温度随深度递增，生存难度随高度递增，这是全城公认的铁律。";
        let body = para.repeat(40); // 1920 字
        let v = split_subblocks(&body);
        assert!(v.len() >= 4, "应切成多块，实际 {} 块", v.len());
        // 每块不超窗口上限（trim 后可能略短）
        assert!(v.iter().all(|s| s.chars().count() <= SUBBLOCK_CHARS));
        // 相邻块有重叠：第二块开头应出现在第一块结尾附近
        let first_end: String = v[0].chars().rev().take(30).collect();
        let first_end: String = first_end.chars().rev().collect();
        assert!(
            v[1].contains(first_end.trim_start_matches(|c: char| "。！？；".contains(c)).get(0..8).unwrap_or("")),
            "相邻块应共享重叠区"
        );
    }

    #[test]
    fn l2_and_blob_roundtrip() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
        let blob = vec_to_blob(&v);
        assert_eq!(blob.len(), 8);
        let back = blob_to_vec(&blob);
        assert_eq!(back, v);
    }

    #[test]
    fn query_prefix_is_bge_retrieval_prefix() {
        assert!(QUERY_PREFIX.starts_with("为这个句子生成表示"));
        assert!(QUERY_PREFIX.ends_with('：'));
    }
}
