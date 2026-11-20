@echo off
cd /d "%~dp0"
echo Glass: view and control your Mac from this PC.
echo On the Mac: open Glass and click "Share This Mac". It shows the Mac's address.
echo The first time, the Mac shows a 6-digit PIN to type here.
echo While viewing: Ctrl+Alt+Shift+F window / full screen, Ctrl+Alt+Shift+Q disconnect.
echo Ctrl acts as the Mac's Cmd (Ctrl+C copies) and Win as Ctrl; Alt is Option.
echo Each session's numbers are saved to results-viewer.txt.
echo.
if exist results-viewer.txt del results-viewer.txt
glass.exe connect --log results-viewer.txt %*
echo.
pause
