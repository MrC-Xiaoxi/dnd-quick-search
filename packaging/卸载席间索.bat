@echo off
rem 本文件需以 GBK(936) 编码 + CRLF 行尾 保存，转换由 package-release.ps1 完成
chcp 936 >nul
setlocal
title 席间索 卸载程序

echo ==================================================
echo    席间索 (Table Canon) 卸载程序
echo ==================================================
echo.
echo 将删除：程序文件、桌面/开始菜单快捷方式、卸载注册项。
echo 保留：你的战役库(.tcs)与导入的资料文件，以及 %APPDATA%\table-canon 下的缓存。
echo.
choice /c YN /m "确认卸载吗 (Y=卸载, N=取消)"
if errorlevel 2 (
    echo 已取消。
    pause
    exit /b 0
)

taskkill /im 席间索.exe /f >nul 2>&1

echo 正在删除快捷方式与注册项...
powershell -NoProfile -ExecutionPolicy Bypass -Command "$d=[Environment]::GetFolderPath('Desktop'); Remove-Item ($d+'\席间索.lnk') -ErrorAction SilentlyContinue; Remove-Item ($env:APPDATA+'\Microsoft\Windows\Start Menu\Programs\席间索.lnk') -ErrorAction SilentlyContinue"
reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\TableCanon" /f >nul 2>&1

set "DIR=%~dp0"
echo 正在删除程序文件...
if exist "%DIR%testdata" rd /s /q "%DIR%testdata" >nul 2>&1
if exist "%DIR%models" rd /s /q "%DIR%models" >nul 2>&1
del /f /q "%DIR%席间索.exe" "%DIR%llmconfig.example.toml" "%DIR%使用说明.txt" "%DIR%onnxruntime.dll" >nul 2>&1

echo.
echo 卸载完成。如目录中还有剩余文件（例如你自己改过的 llmconfig.toml），可手动删除。
echo 如需彻底清理，还可删除: %APPDATA%\table-canon （内含「试用样例」库 demo-sample.tcs，会一并删除；自己新建的库不受影响）
start "" explorer "%DIR%"
(goto) 2>nul & del /f /q "%~f0"
