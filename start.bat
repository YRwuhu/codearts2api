@echo off
setlocal enabledelayedexpansion
cd /d "%~dp0"

set "EXE=target\release\codearts2api.exe"
if not exist "%EXE%" (
    echo [INFO] Release binary not found, building...
    cargo build --release
    if errorlevel 1 (
        echo [ERROR] Build failed.
        pause
        exit /b 1
    )
)

set "ENVFILE=.env"
if not exist "%ENVFILE%" (
    echo [ERROR] .env not found at %ENVFILE%
    echo         Copy .env.example to .env and fill CODEARTS_AK / CODEARTS_SK.
    pause
    exit /b 1
)

set "PORT=8787"
for /f "usebackq tokens=1,* delims==" %%A in ("%ENVFILE%") do (
    if /i "%%A"=="CODEARTS_PROXY_PORT" set "PORT=%%B"
)

netstat -ano | findstr ":%PORT% " | findstr "LISTENING" >nul 2>&1
if not errorlevel 1 (
    echo [ERROR] Port %PORT% is already in use.
    echo         Stop the running instance first, or change CODEARTS_PROXY_PORT in .env.
    pause
    exit /b 1
)

chcp 65001 >nul
set "PYTHONUTF8=1"

echo ============================================
echo  CodeArts2API (Rust)  OpenAI-compatible proxy
echo  Base URL : http://127.0.0.1:%PORT%/v1
echo  API Key  : codearts  (any non-empty string)
echo ============================================
echo.

"%EXE%" serve
set "CODE=%ERRORLEVEL%"
echo.
echo Server exited with code %CODE%.
pause
endlocal
