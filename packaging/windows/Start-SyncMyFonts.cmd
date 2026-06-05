@echo off
setlocal

set "SCRIPT_DIR=%~dp0"
set "AGENT=%SCRIPT_DIR%bin\syncmyfonts-agent.exe"
if exist "%AGENT%" goto run

set "AGENT=%SCRIPT_DIR%..\..\bin\syncmyfonts-agent.exe"
if exist "%AGENT%" goto run

echo Could not find bin\syncmyfonts-agent.exe next to this launcher.
echo Move this launcher back into the SyncMyFonts release folder and try again.
pause
exit /b 1

:run
"%AGENT%" app
