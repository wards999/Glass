@echo off
cd /d "%~dp0"
echo Glass M1 test: records two 10-second clips of your main display.
echo Use the desktop normally while it records (scroll text, drag a window, minimise/restore).
echo.
pause
(
  glass.exe --version
  glass.exe list
  echo.
  glass.exe record --seconds 10 --chroma 420 desktop-420.glsr
  echo.
  glass.exe record --seconds 10 --chroma 444 desktop-444.glsr
) > results-win.txt 2>&1
type results-win.txt
echo.
echo Saved results-win.txt. Copy desktop-420.glsr and desktop-444.glsr to the Mac.
pause
