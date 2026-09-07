#!/usr/bin/env bash
# 一次性下载语义检索所需模型与 ONNX Runtime（本机开发/打包用；产物不进 git）
# 用法: bash scripts/fetch-model.sh
# 来源：
#   模型   https://hf-mirror.com/Xenova/bge-small-zh-v1.5 (INT8 量化 onnx + tokenizer.json)
#   运行时 nuget 镜像 Microsoft.ML.OnnxRuntime 1.22.0 (win-x64 native dll)
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p models/bge-small-zh-v1.5 models/onnxruntime

echo "[1/3] 下载 bge-small-zh-v1.5 INT8 模型 (~24MB)..."
curl -L --fail --retry 3 -o models/bge-small-zh-v1.5/model.onnx \
  "https://hf-mirror.com/Xenova/bge-small-zh-v1.5/resolve/main/onnx/model_quantized.onnx"

echo "[2/3] 下载 tokenizer.json..."
curl -L --fail --retry 3 -o models/bge-small-zh-v1.5/tokenizer.json \
  "https://hf-mirror.com/Xenova/bge-small-zh-v1.5/resolve/main/tokenizer.json"

echo "[3/3] 下载 onnxruntime 1.22.0 (win-x64)..."
curl -L --fail --retry 3 -o models/onnxruntime/ort.nupkg \
  "https://nuget.azure.cn/v3-flatcontainer/microsoft.ml.onnxruntime/1.22.0/microsoft.ml.onnxruntime.1.22.0.nupkg"
powershell -NoProfile -Command "Copy-Item 'models/onnxruntime/ort.nupkg' 'models/onnxruntime/ort.zip' -Force; Expand-Archive -Force 'models/onnxruntime/ort.zip' 'models/onnxruntime/nupkg'"
cp "models/onnxruntime/nupkg/runtimes/win-x64/native/onnxruntime.dll" models/onnxruntime/onnxruntime.dll
rm -rf models/onnxruntime/nupkg models/onnxruntime/ort.nupkg

echo "完成："
ls -la models/bge-small-zh-v1.5 models/onnxruntime
