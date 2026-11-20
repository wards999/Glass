@echo off
cd /d "%~dp0"
if not exist glass.exe (
  echo glass.exe isn't next to this file. Windows runs a .cmd straight from a zip
  echo without the rest of it: right-click the zip, Extract All, then run it from
  echo the extracted folder.
  pause
  exit /b 1
)
rem Ask for administrator rights once, so the Mac can also control admin
rem windows (Task Manager, installers): Windows drops input into them from a
rem program that isn't elevated. If the user says No, carry on without.
rem LATER(M7): a service for UAC prompts and the lock screen.
fltmc >nul 2>&1
if not errorlevel 1 goto run
echo Asking Windows for administrator rights (answer Yes to control admin windows) ...
set "GLASS_SELF=%~f0"
set "GLASS_ARGS=%*"
powershell -NoProfile -Command "$a=@{FilePath=$env:GLASS_SELF; Verb='RunAs'; ErrorAction='Stop'}; if ($env:GLASS_ARGS) { $a.ArgumentList=$env:GLASS_ARGS }; try { Start-Process @a } catch { exit 1 }"
if not errorlevel 1 exit /b
echo Not elevated: continuing here. Input into admin windows will be blocked.
echo.

:run
echo Glass: streams this PC's main display live to the Mac.
echo If Windows Firewall asks, allow glass.exe on Private networks.
echo Each session's numbers are saved to results-host.txt. Close this window to stop.
echo The Mac's mouse and keyboard control this PC while connected.
echo Extra options pass through, e.g.:  host.cmd --chroma 444
echo.
if exist results-host.txt del results-host.txt
glass.exe host --log results-host.txt %*
echo.
pause
