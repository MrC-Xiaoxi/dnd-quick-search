@echo off
rem 本文件需以 GBK(936) 编码 + CRLF 行尾 保存，转换由 package-release.ps1 完成
chcp 936 >nul
setlocal
title 席间索 安装程序

echo ==================================================
echo    席间索 (Table Canon) v0.1.0 安装程序
echo    本地跑团资料检索 · 免管理员权限 · 随时可卸载
echo ==================================================
echo.

set "SRCDIR=%~dp0"
set "DEFDIR=%LOCALAPPDATA%\Programs\TableCanon"

rem 升级安装时先关掉正在运行的程序
taskkill /im 席间索.exe /f >nul 2>&1

echo 请选择安装目录。
echo 直接回车 = %DEFDIR%
echo.
set /p "TARGET=安装目录: "
if "%TARGET%"=="" set "TARGET=%DEFDIR%"
set "TARGET=%TARGET:"=%"
if "%TARGET:~-1%"=="\" set "TARGET=%TARGET:~0,-1%"

echo.
echo 正在安装到: %TARGET%
if not exist "%TARGET%" (
    mkdir "%TARGET%" 2>nul
    if errorlevel 1 (
        echo [错误] 无法创建目录，请检查路径是否可写。
        pause
        exit /b 1
    )
)

copy /y "%SRCDIR%席间索.exe" "%TARGET%\" >nul
if errorlevel 1 (
    echo [错误] 文件复制失败，请确认程序未在运行、目录可写。
    pause
    exit /b 1
)
copy /y "%SRCDIR%llmconfig.example.toml" "%TARGET%\" >nul
if exist "%SRCDIR%使用说明.txt" copy /y "%SRCDIR%使用说明.txt" "%TARGET%\" >nul
if exist "%SRCDIR%卸载席间索.bat" copy /y "%SRCDIR%卸载席间索.bat" "%TARGET%\" >nul
if exist "%SRCDIR%testdata" robocopy "%SRCDIR%testdata" "%TARGET%\testdata" /e /nfl /ndl /njh >nul

echo 正在创建快捷方式（桌面 + 开始菜单）...
powershell -NoProfile -ExecutionPolicy Bypass -Command "$w=New-Object -ComObject WScript.Shell; $d=[Environment]::GetFolderPath('Desktop'); $l=$w.CreateShortcut($d+'\席间索.lnk'); $l.TargetPath='%TARGET%\席间索.exe'; $l.WorkingDirectory='%TARGET%'; $l.Description='席间索 - 跑团资料席间检索'; $l.Save(); $p=$env:APPDATA+'\Microsoft\Windows\Start Menu\Programs\席间索.lnk'; $l2=$w.CreateShortcut($p); $l2.TargetPath='%TARGET%\席间索.exe'; $l2.WorkingDirectory='%TARGET%'; $l2.Save()"

echo 正在注册卸载入口（Windows 设置 - 应用）...
set "UK=HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\TableCanon"
reg add "%UK%" /v DisplayName    /d "席间索 (Table Canon)" /f >nul
reg add "%UK%" /v DisplayVersion /d "0.1.0" /f >nul
reg add "%UK%" /v DisplayIcon    /d "%TARGET%\席间索.exe" /f >nul
reg add "%UK%" /v InstallLocation /d "%TARGET%" /f >nul
reg add "%UK%" /v Publisher      /d "Table Canon" /f >nul
reg add "%UK%" /v NoModify       /d 1 /f >nul
reg add "%UK%" /v NoRepair       /d 1 /f >nul
reg add "%UK%" /v UninstallString /d "\"%TARGET%\卸载席间索.bat\"" /f >nul

echo.
echo ==================================================
echo   安装完成！
echo   - 桌面和开始菜单已创建「席间索」快捷方式
echo   - 卸载：Windows 设置 - 应用 - 席间索，或运行
echo     %TARGET%\卸载席间索.bat
echo   - 卸载不会删除你建的战役库(.tcs)和导入的资料
echo ==================================================
pause
