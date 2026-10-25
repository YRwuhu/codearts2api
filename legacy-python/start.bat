@echo off
setlocal enabledelayedexpansion
cd /d "%~dp0"

if not exist ".venv\Scripts\python.exe" (
    echo [ERROR] Virtual environment not found: .venv\Scripts\python.exe
    echo         Run: python -m venv .venv
    echo         Then: .venv\Scripts\python.exe -m pip install -r requirements.txt
    pause
    exit /b 1
)

if not exist ".env" (
    echo [ERROR] .env not found.
    echo         Copy .env.example to .env and fill CODEARTS_AK / CODEARTS_SK.
    pause
    exit /b 1
)

rem --- resolve listen port from .env (fallback 8787) ---
set "PORT=8787"
for /f "usebackq tokens=1,* delims==" %%A in (".env") do (
    if /i "%%A"=="CODEARTS_PROXY_PORT" set "PORT=%%B"
)

rem --- refuse to start if the port is already taken ---
netstat -ano | findstr ":%PORT% " | findstr "LISTENING" >nul 2>&1
if not errorlevel 1 (
    echo [ERROR] Port %PORT% is already in use.
    echo         Stop the running instance first, or change CODEARTS_PROXY_PORT in .env.
    pause
    exit /b 1
)

rem --- force console and Python to agree on UTF-8 ---
chcp 65001 >nul
set "PYTHONUTF8=1"
set "PYTHONIOENCODING=utf-8"

echo ============================================
echo  CodeArts2API  OpenAI-compatible proxy
echo  Base URL : http://127.0.0.1:%PORT%/v1
echo  API Key  : codearts  (any non-empty string)
echo ============================================
echo.

".venv\Scripts\python.exe" -u server.py
set "CODE=%ERRORLEVEL%"
echo.
echo Server exited with code %CODE%.
pause
endlocal
