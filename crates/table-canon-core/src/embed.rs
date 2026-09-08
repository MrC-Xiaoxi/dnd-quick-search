//! M2 语义检索：BGE-small-zh-v1.5 的 ONNX 编码器与语义子块切分。
//! 口径来自技术方案 §7.1/§7.6：CLS pooling、L2 归一、查询侧前缀、512 token 截右侧、CPU 2 线程、batch=1。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// model.onnx 内容的 sha256。`MODEL_ID` 是编译期常量，换模型文件（哪怕同名同版本）
    /// 也变不了，只比它等于没比；指纹写进 embedding_meta 才是真校验（§7.5）。
    fingerprint: String,
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

        // 内容指纹：换过模型文件但沿用同一 MODEL_ID 时，靠它发现「库里的向量不是这个模型算的」
        let fingerprint = crate::normalize::sha256_file_hex(
            &std::fs::read(&model)
                .with_context(|| format!("读取模型失败: {}", model.display()))?,
        );

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
            fingerprint,
        })
    }

    pub fn model_id(&self) -> &'static str {
        MODEL_ID
    }

    /// 模型内容指纹（model.onnx 的 sha256），供 embedding_meta 强校验比对。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// 文档侧编码：无前缀（§7.1）。
    pub fn encode_doc(&self, text: &str) -> Result<Vec<f32>> {
        self.encode(text)
    }

    /// 查询侧编码：带 BGE 检索前缀（§7.1）。
    pub fn encode_query(&self, text: &str) -> Result<Vec<f32>> {
        let text = text.trim();
        // 空查询必须在这里拦：拼上前缀后整体非空，encode 的空文本守卫就失效了
        if text.is_empty() {
            bail!("空查询不编码");
        }
        self.encode(&format!("{QUERY_PREFIX}{text}"))
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
        let out_names: Vec<String> = session.outputs.iter().map(|o| o.name.clone()).collect();
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
        // 用 get 而不是 outputs["..."]：后者在输出名不符时直接 panic，
        // 会把「模型不兼容」变成进程崩溃，绕过 §7.3 的优雅降级
        let out = outputs.get("last_hidden_state").ok_or_else(|| {
            anyhow::anyhow!("模型没有 last_hidden_state 输出（实际输出：{out_names:?}）")
        })?;
        let (shape, data) = out.try_extract_tensor::<f32>()?;
        // 布局 (1, seq, dim) 行主序：长度须被 token 数整除，且隐层维度须等于 512。
        // 只查 len < 512 会放过 768/1024 维模型——那会静默截前 512 维当向量用。
        if n == 0 || data.len() % n != 0 {
            bail!(
                "模型输出长度 {} 与 token 数 {n} 不整除（输出不是 (1,seq,dim) 布局？）: shape={shape:?}",
                data.len()
            );
        }
        let dim = data.len() / n;
        if dim != EMBED_DIM {
            bail!("模型隐层维度 {dim} ≠ {EMBED_DIM}（models/ 下换成了别的模型？）: shape={shape:?}");
        }
        // CLS pooling：第一个 token 的隐状态（§7.1，不是 mean pooling）
        let mut v: Vec<f32> = data[..EMBED_DIM].to_vec();
        l2_normalize(&mut v);
        Ok(v)
    }
}

/// load-dynamic 模式下 onnxruntime.dll 的定位只需设置一次。
/// 用互斥量而不是 OnceLock 的 check-then-act：两个线程同时首次加载时，
/// 后者可能在前者 set_var 之前就判定「已初始化」而跳过（且 set_var 本身非线程安全）。
fn init_ort_dll(dll_path: Option<&Path>) -> Result<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if std::env::var_os("ORT_DYLIB_PATH")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let resolved = dll_path.map(Path::to_string_lossy).map(|p| p.to_string());
    if let Some(p) = resolved {
        if !Path::new(&p).is_file() {
            bail!("onnxruntime.dll 不存在: {p}");
        }
        std::env::set_var("ORT_DYLIB_PATH", &p);
    }
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
    candidate_roots_from(
        std::env::current_exe().ok().as_deref(),
        std::env::current_dir().ok(),
    )
}

/// 拆出来便于测试。exe 在 target/ 下即开发态，此时向上找仓库根（模型在仓库的 models/）。
fn candidate_roots_from(exe: Option<&Path>, cwd: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(exe) = exe {
        if let Some(parent) = exe.parent() {
            out.push(parent.to_path_buf());
            // 开发态：cargo run 的 exe 在 target/<profile>/ 下，而模型在仓库根的 models/。
            // 只在路径里出现过 target 时向上找，安装包布局不受影响。
            if parent
                .ancestors()
                .any(|a| a.file_name().is_some_and(|n| n == "target"))
            {
                out.extend(parent.ancestors().skip(1).take(3).map(Path::to_path_buf));
            }
        }
    }
    // 从仓库根直接运行（run-demo.bat 的 CWD 即仓库根）
    if let Some(cwd) = cwd {
        out.push(cwd);
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

/// 直接对 BLOB 求点积，省掉每行一次 `blob_to_vec` 分配（检索热路径）。
/// 字节数与查询维度不符时返回 None（调用方跳过该行）。
pub fn dot_blob(q: &[f32], blob: &[u8]) -> Option<f32> {
    if blob.len() != q.len() * 4 {
        return None;
    }
    Some(
        q.iter()
            .zip(blob.chunks_exact(4))
            .map(|(x, c)| x * f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .sum(),
    )
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
        // 相邻块有重叠：前一块的末尾若干字符必须出现在下一块里。
        // 注意按「字符」切：中文的字节下标落在字符中间，get(0..8) 会返回 None，
        // 再 unwrap_or("") 就退化成 contains("") 恒真——这个断言曾因此形同虚设。
        let tail: String = {
            let cs: Vec<char> = v[0].chars().collect();
            cs[cs.len().saturating_sub(8)..].iter().collect()
        };
        let tail = tail.trim();
        assert!(!tail.is_empty(), "块尾取样为空，断言无意义");
        assert!(
            v[1].contains(tail),
            "相邻块应共享重叠区：v[0] 末尾 {tail:?} 未出现在 v[1] 中"
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
    fn dev_build_roots_walk_up_from_target() {
        // 开发态：cargo run 的 exe 在 target/release 下，模型在仓库根 models/
        let exe = PathBuf::from("C:/work/repo/target/release/app.exe");
        let roots = candidate_roots_from(Some(&exe), None);
        assert!(
            roots.iter().any(|r| r == &PathBuf::from("C:/work/repo")),
            "应上溯到仓库根，实际 {roots:?}"
        );
        // 安装包布局：exe 就在模型旁边，不应上溯出无关目录
        let installed = PathBuf::from("C:/Program Files/table-canon/app.exe");
        let roots2 = candidate_roots_from(Some(&installed), None);
        assert_eq!(
            roots2.first(),
            Some(&PathBuf::from("C:/Program Files/table-canon"))
        );
        assert!(roots2.len() <= 2, "不应上溯出无关目录: {roots2:?}");
    }

    #[test]
    fn query_prefix_is_bge_retrieval_prefix() {
        assert!(QUERY_PREFIX.starts_with("为这个句子生成表示"));
        assert!(QUERY_PREFIX.ends_with('：'));
    }
}
