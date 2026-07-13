@echo off
rem GG Browser - tkinter shell (double-click to run)
cd /d "%~dp0"
"%LOCALAPPDATA%\Programs\Python\Python312\python.exe" main.py %*
if errorlevel 1 pause
