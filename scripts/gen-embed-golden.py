#!/usr/bin/env python3
"""生成 §7.1.1 golden 参考向量（官方实现口径：CLS pooling + L2 归一，查询句加前缀）。

用法（一次性，需 python 环境）：

    pip install torch transformers
    HF_ENDPOINT=https://hf-mirror.com python scripts/gen-embed-golden.py \
        --model BAAI/bge-small-zh-v1.5 --out testdata/embed_golden.json

产物由 crates/table-canon-core/tests/embed_golden.rs 消费：产品 ONNX 路径对同一批
句子出向量，逐句余弦须 ≥ threshold。换模型或改口径（pooling / 前缀 / 归一）后必须重新生成。

门槛校准：方案 §7.1.1 给 INT8 定的是 0.995，但 bge-small-zh-v1.5 的 INT8 产物实测
最差 0.9878（FP32 产物 0.999999，说明导出无误、纯量化损失），故默认取 0.98。
详见 docs/m2-语义检索说明.md。
"""

import argparse
import json
from pathlib import Path

# 与 crates/table-canon-core/src/embed.rs 的 QUERY_PREFIX 必须逐字一致
QUERY_PREFIX = "为这个句子生成表示以用于检索相关文章："

# (文本, 是否按查询编码)。≥10 句，覆盖正文句与口语查询句。
CASES = [
    ("铁砧堡扼守灰鹰山口的北麓，城墙由黑曜岩砌成。", False),
    ("城主荀岚出身铁卫团，兼管税收与熔炉区治安。", False),
    ("登记员格里姆在南门塔楼办公，负责记录每日伤亡与悬赏发放。", False),
    ("熔炉区商会每旬开一次集会，现任会长是矮人巴尔多。", False),
    ("山口下方的旧驿道通往废弃的哨站，据说埋着前朝的军械。", False),
    ("断桅酒馆的招牌是一截烧焦的桅杆，据说来自沉没的潮响号。", False),
    ("独眼酒保给过玩家一条关于走私路线的线索。", False),
    ("潮响号在风暴季触礁沉没，船员无一生还。", False),
    ("想找个地方小酌一杯，听说有家酒馆招牌挺特别", True),
    ("谁负责记录伤亡和发放悬赏？", True),
    ("山口的过路税是谁在管？", True),
    ("哪里能打听到走私的消息？", True),
]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="BAAI/bge-small-zh-v1.5", help="HF 模型 id 或本地目录")
    ap.add_argument("--label", default="BAAI/bge-small-zh-v1.5", help="写入产物的模型标识")
    ap.add_argument("--out", default="testdata/embed_golden.json")
    ap.add_argument("--quant", default="int8", help="产物量化档：int8 / fp32")
    ap.add_argument("--threshold", type=float, default=0.98, help="默认按本模型 INT8 实测校准，见模块说明")
    ap.add_argument("--digits", type=int, default=6, help="向量小数位（缩体积，远高于门槛精度）")
    args = ap.parse_args()

    import torch
    from transformers import AutoModel, AutoTokenizer

    tokenizer = AutoTokenizer.from_pretrained(args.model)
    model = AutoModel.from_pretrained(args.model)
    model.eval()

    cases = []
    for text, as_query in CASES:
        payload = QUERY_PREFIX + text if as_query else text
        enc = tokenizer(payload, return_tensors="pt", truncation=True, max_length=512)
        with torch.no_grad():
            hidden = model(**enc).last_hidden_state
        vec = hidden[:, 0]  # CLS pooling：官方 BGE 取首 token，不是 mean
        vec = torch.nn.functional.normalize(vec, p=2, dim=1)
        cases.append(
            {
                "text": text,
                "as_query": as_query,
                "vector": [round(x, args.digits) for x in vec[0].tolist()],
            }
        )

    doc = {
        "quant": args.quant,
        "threshold": args.threshold,
        "model": args.label,
        "pooling": "cls",
        "query_prefix": QUERY_PREFIX,
        "cases": cases,
    }
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(doc, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"wrote {out}（{len(cases)} 句，dim={len(cases[0]['vector'])}，threshold={args.threshold}）")


if __name__ == "__main__":
    main()
