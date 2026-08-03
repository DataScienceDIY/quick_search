@echo off
rem Set up the build environment, build QuickSearch, and launch the GUI. The
rem Windows counterpart of build.sh; run "build.bat --help" for usage.
rem
rem Terminal search is a separate binary on Windows, because the GUI is built
rem as a window-subsystem app and cannot write to the calling shell:
rem     target\release\quicksearch-cli.exe --help
rem
rem A fresh machine needs three things, all installed here when missing and
rem skipped when already present:
rem   * the MSVC C++ build tools - SQLCipher, zstd and OpenSSL are compiled
rem     from bundled C sources, so a C toolchain is required;
rem   * Perl - OpenSSL's Configure is a Perl script, and rusqlite's
rem     bundled-sqlcipher-vendored-openssl feature builds OpenSSL;
rem   * the Rust toolchain, via rustup.
rem NASM is optional: openssl-src uses it for assembly optimisations when it is
rem on PATH and builds without it otherwise.
setlocal EnableExtensions

set "ROOT=%~dp0"
set "DO_RUN=1"
set "MODE=run"
set "ARGS="
set "MISSING="

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--no-run" (
    set "DO_RUN=0"
    shift
    goto parse
)
if /i "%~1"=="--check" (
    set "MODE=check"
    shift
    goto parse
)
if /i "%~1"=="-h" goto help
if /i "%~1"=="--help" goto help
if "%~1"=="--" (
    shift
    goto collect
)
rem Not ours: this argument and everything after it belongs to the binary.
goto collect

rem shift does not rewrite %*, so the binary's arguments are re-accumulated by
rem hand. Each pass re-parses this line, so %ARGS% is always the current value.
:collect
if "%~1"=="" goto parsed
set "ARGS=%ARGS% "%~1""
shift
goto collect

:help
echo Set up the build environment, build QuickSearch, and launch the GUI.
echo.
echo   build.bat                install what is missing, build release, launch
echo   build.bat --no-run       stop after the build
echo   build.bat --check        report dependency status; install and build nothing
echo   build.bat -- ^<args^>      pass everything after -- to the launched binary
echo.
echo Anything unrecognised ends option parsing and reaches the binary; use --
echo for arguments that look like flags.
exit /b 0

:parsed
set "WINGET="
where winget >nul 2>&1 && set "WINGET=1"

rem ------------------------------------------------------------- MSVC ------

where cl >nul 2>&1 && goto msvc_ok
rem cl is only on PATH inside a developer prompt, so an ordinary shell asks
rem vswhere instead. cargo (through cc-rs) locates MSVC the same way, which is
rem why nothing has to be added to PATH after installing it.
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
set "VCDIR="
if not exist "%VSWHERE%" goto msvc_missing
for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2^>nul`) do set "VCDIR=%%i"
if defined VCDIR goto msvc_ok

:msvc_missing
if "%MODE%"=="check" (
    echo     MSVC C++ build tools: MISSING
    set "MISSING=1"
    goto msvc_done
)
if not defined WINGET goto msvc_manual
echo ==^> Installing Visual Studio 2022 Build Tools ^(a multi-gigabyte download^)
rem --includeRecommended is what pulls in the Windows SDK alongside VCTools.
winget install -e --id Microsoft.VisualStudio.2022.BuildTools --accept-source-agreements --accept-package-agreements --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
if errorlevel 1 goto msvc_manual
goto msvc_done

:msvc_manual
echo build: the MSVC C++ toolchain is required. Install it with: >&2
echo     winget install -e --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended" >&2
echo   or get the Build Tools from https://visualstudio.microsoft.com/downloads/ >&2
echo   and select the "Desktop development with C++" workload. >&2
exit /b 1

:msvc_ok
if "%MODE%"=="check" echo     MSVC C++ build tools: ok
:msvc_done

rem ------------------------------------------------------------- Perl ------

where perl >nul 2>&1 && goto perl_ok
if exist "C:\Strawberry\perl\bin\perl.exe" goto perl_path
if "%MODE%"=="check" (
    echo     Perl: MISSING
    set "MISSING=1"
    goto perl_done
)
if not defined WINGET goto perl_manual
echo ==^> Installing Strawberry Perl
winget install -e --id StrawberryPerl.StrawberryPerl --accept-source-agreements --accept-package-agreements
if errorlevel 1 goto perl_manual
if not exist "C:\Strawberry\perl\bin\perl.exe" goto perl_manual

:perl_path
rem A just-installed package is not on the PATH of this already-running shell.
set "PATH=C:\Strawberry\perl\bin;C:\Strawberry\c\bin;%PATH%"
goto perl_done

:perl_manual
echo build: Perl is required ^(OpenSSL's Configure is a Perl script^). Install it with: >&2
echo     winget install -e --id StrawberryPerl.StrawberryPerl >&2
echo   or get it from https://strawberryperl.com/ >&2
exit /b 1

:perl_ok
if "%MODE%"=="check" echo     Perl: ok
:perl_done

rem ------------------------------------------------------------- NASM ------

where nasm >nul 2>&1 && goto nasm_ok
if exist "%ProgramFiles%\NASM\nasm.exe" goto nasm_path
if "%MODE%"=="check" (
    echo     NASM: missing ^(optional^)
    goto nasm_done
)
if not defined WINGET goto nasm_skip
echo ==^> Installing NASM ^(optional: OpenSSL assembly optimisations^)
winget install -e --id NASM.NASM --accept-source-agreements --accept-package-agreements
if errorlevel 1 goto nasm_skip
if not exist "%ProgramFiles%\NASM\nasm.exe" goto nasm_skip

:nasm_path
set "PATH=%ProgramFiles%\NASM;%PATH%"
goto nasm_done

rem Never fatal: OpenSSL falls back to a build without assembly optimisations.
:nasm_skip
echo build: NASM unavailable; OpenSSL will build without assembly optimisations.
goto nasm_done

:nasm_ok
if "%MODE%"=="check" echo     NASM: ok
:nasm_done

rem ------------------------------------------------------------- Rust ------

where cargo >nul 2>&1 && goto cargo_ok
rem rustup edits the user's PATH, which an already-running shell never sees.
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" goto cargo_path
if "%MODE%"=="check" (
    echo     Rust: MISSING ^(rustup would install it^)
    set "MISSING=1"
    goto cargo_done
)
where curl >nul 2>&1 || goto rust_manual
echo ==^> Installing the Rust toolchain ^(rustup^)
curl -sSfLo "%TEMP%\rustup-init.exe" https://win.rustup.rs/x86_64
if errorlevel 1 goto rust_manual
"%TEMP%\rustup-init.exe" -y --profile minimal --default-toolchain stable-x86_64-pc-windows-msvc
if errorlevel 1 goto rust_manual
del "%TEMP%\rustup-init.exe" >nul 2>&1
if not exist "%USERPROFILE%\.cargo\bin\cargo.exe" goto rust_manual

:cargo_path
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
goto cargo_done

:rust_manual
echo build: could not install Rust automatically; get it from https://rustup.rs/ >&2
exit /b 1

:cargo_ok
if "%MODE%"=="check" for /f "tokens=*" %%v in ('cargo --version') do echo     Rust: %%v
:cargo_done

rem ------------------------------------------------------------ build ------

if "%MODE%"=="check" (
    if defined MISSING exit /b 1
    echo ==^> Ready to build
    exit /b 0
)

rem %~dp0 is this script's own directory (with a trailing backslash), so neither
rem the build nor the launch depends on the current working directory.
cd /d "%ROOT%"
cargo build --release -p quicksearch-gui
if errorlevel 1 exit /b 1

if "%DO_RUN%"=="0" (
    echo ==^> Built target\release\quicksearch.exe and target\release\quicksearch-cli.exe
    exit /b 0
)

"%ROOT%target\release\quicksearch.exe"%ARGS%
