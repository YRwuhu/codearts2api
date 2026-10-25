@echo off
setlocal
cd /d "%~dp0"

echo Building release binary...
cargo build --release
if errorlevel 1 (
    echo.
    echo [ERROR] Build failed.
    pause
    exit /b 1
)

echo.
echo [OK] target\release\codearts2api.exe
dir /b target\release\codearts2api.exe
pause
endlocal
