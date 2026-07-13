@echo off
rem GG Browser - native Rust (winit) window shell, own rasterizer
cd /d "%~dp0"
"%LOCALAPPDATA%\Programs\Python\Python312\python.exe" main.py --native %*
if errorlevel 1 pause
