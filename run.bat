@echo off
rem Build and launch the QuickSearch GUI, mirroring run.sh.
rem Terminal search is a separate binary on Windows, because the GUI is built
rem as a window-subsystem app and cannot write to the calling shell:
rem     target\release\quicksearch-cli.exe --help
setlocal
cargo build --release -p quicksearch-gui
if errorlevel 1 exit /b 1
rem %~dp0 is this script's own directory (with a trailing backslash), so the
rem launch does not depend on the current working directory.
"%~dp0target\release\quicksearch.exe" %*
