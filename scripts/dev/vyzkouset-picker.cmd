@echo off
rem Double-click: prepares the Open-dialog picker test (scripts\dev\try-picker.ps1).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0try-picker.ps1"
echo.
pause
