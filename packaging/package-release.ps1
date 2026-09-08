# 席间索 Windows 发布打包脚本
# 用法: powershell -NoProfile -ExecutionPolicy Bypass -File packaging\package-release.ps1
# 产物: dist\席间索-v<版本>-win64.zip（解压即用；内含安装/卸载脚本）
# 说明:
#   - 构建叠加 crt-static，exe 自带 C 运行时，目标机器无需安装 VC++ Redist
#   - packaging/*.bat 为 GBK+CRLF、*.txt/*.ps1 为 UTF8-BOM（cmd 与 PowerShell 5.1 解析中文所必需）
#     转换是幂等的：写回前先剥掉已有的 BOM，否则每跑一次就多一个 EF BB BF

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$gbk = [Text.Encoding]::GetEncoding(936)
$utf8bom = New-Object Text.UTF8Encoding $true
function Convert-Text([string]$rel, [bool]$bom) {
    $p = Join-Path $root $rel
    $strict = New-Object Text.UTF8Encoding -ArgumentList $false, $true  # 非法 UTF-8 字节即抛异常
    try {
        $text = $strict.GetString([IO.File]::ReadAllBytes($p)) -replace "(?<!`r)`n", "`r`n"
        # 已有 BOM 会被解成 U+FEFF，WriteAllText 又会补一个：不剥掉就每次运行累加一层
        $text = $text.TrimStart([char]0xFEFF)
        [IO.File]::WriteAllText($p, $text, $(if ($bom) { $utf8bom } else { $gbk }))
        Write-Host "  编码转换: $rel"
    } catch {
        Write-Host "  已是目标编码，跳过: $rel"
    }
}

# 1) 编码规范化（幂等）
Write-Host "== 编码规范化 =="
Convert-Text "packaging\安装席间索.bat" $false
Convert-Text "packaging\卸载席间索.bat" $false
Convert-Text "packaging\使用说明.txt" $true
Convert-Text "packaging\package-release.ps1" $true

# 读取版本号（apps/desktop/Cargo.toml）
$crate = Get-Content (Join-Path $root "apps\desktop\Cargo.toml") -Raw -Encoding UTF8
$ver = if ($crate -match '(?m)^\s*version\s*=\s*"([^"]+)"') { $Matches[1] } else { "0.1.0" }
Write-Host "== 席间索 发布打包 v$ver =="

# 2) 静态 CRT release 构建
Push-Location $root
try {
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
    Write-Host "== cargo build --release（含 lto，约 1-3 分钟）=="
    cargo build --release -p table-canon-desktop
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }
    Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue

    # 3) 组装发布目录
    $stage = Join-Path $root "dist\席间索-v$ver"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force -Path (Join-Path $stage "testdata") | Out-Null
    Copy-Item "target\release\table-canon-desktop.exe" (Join-Path $stage "席间索.exe")
    Copy-Item "llmconfig.example.toml" $stage
    Copy-Item "packaging\安装席间索.bat" $stage
    Copy-Item "packaging\卸载席间索.bat" $stage
    Copy-Item "packaging\使用说明.txt" $stage
    Copy-Item "testdata\sample-campaign" (Join-Path $stage "testdata\sample-campaign") -Recurse -Force
    Copy-Item "README.md" $stage -ErrorAction SilentlyContinue

    # M2 语义检索：模型与 ONNX Runtime 必须成对进包（缺任一个都跑不起来）
    $hasModel = (Test-Path "models\bge-small-zh-v1.5\model.onnx") -and (Test-Path "models\bge-small-zh-v1.5\tokenizer.json")
    $hasDll = Test-Path "models\onnxruntime\onnxruntime.dll"
    if ($hasModel -and $hasDll) {
        New-Item -ItemType Directory -Force -Path (Join-Path $stage "models\bge-small-zh-v1.5") | Out-Null
        Copy-Item "models\bge-small-zh-v1.5\model.onnx" (Join-Path $stage "models\bge-small-zh-v1.5\model.onnx")
        Copy-Item "models\bge-small-zh-v1.5\tokenizer.json" (Join-Path $stage "models\bge-small-zh-v1.5\tokenizer.json")
        Copy-Item "models\onnxruntime\onnxruntime.dll" (Join-Path $stage "onnxruntime.dll")
        Write-Host "  语义模型 + onnxruntime.dll 已入包"
    } elseif ($hasModel -or $hasDll) {
        Write-Host "  [提示] 模型与 onnxruntime.dll 只到齐一个，本包不含语义检索（两者必须同时具备）"
    } else {
        Write-Host "  [提示] models\ 缺失，本包不含语义检索（bash scripts/fetch-model.sh 可下载）"
    }

    # 4) 压缩
    $zip = Join-Path $root "dist\席间索-v$ver-win64.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path $stage -DestinationPath $zip

    $mb = "{0:N1}" -f ((Get-Item $zip).Length / 1MB)
    Write-Host "== 打包完成 =="
    Write-Host "  目录: $stage"
    Write-Host "  压缩包: $zip ($mb MB)"
} finally { Pop-Location }
