@echo off
cd /d "%~dp0"
echo Glass: streams this PC's main display live to the Mac.
echo If Windows Firewall asks, allow glass.exe on Private networks.
echo Each session's numbers are saved to results-host.txt. Close this window to stop.
echo The Mac's mouse and keyboard control this PC while connected.
echo For admin programs: right-click host.cmd ^> Run as administrator.
echo Extra options pass through, e.g.:  host.cmd --chroma 444
echo.
if exist results-host.txt del results-host.txt
glass.exe host --log results-host.txt %*
echo.
pause
