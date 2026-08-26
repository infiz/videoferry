@echo off
setlocal

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-windows.ps1" %*
set "BUILD_EXIT_CODE=%ERRORLEVEL%"

if not "%BUILD_EXIT_CODE%"=="0" (
    echo.
    echo Windows package build failed with exit code %BUILD_EXIT_CODE%.
)

exit /b %BUILD_EXIT_CODE%
