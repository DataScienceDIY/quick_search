;
; NSIS installer for QuickSearch.
;
; Compiled by packaging/build-installer.sh, which supplies every define below
; and stages the files this installs. Running makensis on this file by hand
; will fail on the !error checks rather than build something half-configured.
;
;   VERSION       workspace version, e.g. 0.9.1
;   VERSION_QUAD  the same padded to a.b.c.d, which is all VIProductVersion takes
;   STAGE         directory holding the exact files to install
;   OUTFILE       path of the installer to write
;
; makensis runs on Linux, so the installer comes out of the same cross-compile
; job as the .zip and no Windows machine is involved anywhere in the pipeline.
; That also means file paths below use forward slashes: they are read by
; makensis on the build host, unlike $INSTDIR paths, which are Windows strings.

!ifndef VERSION | VERSION_QUAD | STAGE | OUTFILE
    !error "build this with packaging/build-installer.sh, which defines VERSION, VERSION_QUAD, STAGE and OUTFILE"
!endif

Unicode true
; The payload is two ~20 MB binaries; solid LZMA is worth the compression time.
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"

!define APP        "QuickSearch"
!define PUBLISHER  "Jeremy <jeremy@karsttech.com>"
!define HOMEPAGE   "https://code.karsttech.com/jeremy/quick_search"
; Where Add/Remove Programs looks, and where an upgrade finds the directory the
; previous version went into.
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"

Name "${APP} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP}"
; Program Files and HKLM both need elevation, and it is what puts the shortcuts
; in front of every account on the machine rather than just the installing one.
RequestExecutionLevel admin
ShowInstDetails show
ShowUninstDetails show

VIProductVersion "${VERSION_QUAD}"
VIAddVersionKey "ProductName"     "${APP}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "FileVersion"     "${VERSION_QUAD}"
VIAddVersionKey "CompanyName"     "${PUBLISHER}"
VIAddVersionKey "LegalCopyright"  "GPL-3.0-or-later"
VIAddVersionKey "FileDescription" "${APP} ${VERSION} installer"

!define MUI_ABORTWARNING
!define MUI_ICON   "${STAGE}/quicksearch.ico"
!define MUI_UNICON "${STAGE}/quicksearch.ico"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${STAGE}/LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Run ${APP}"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchAsUser
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ------------------------------------------------------------------ init ----

Function .onInit
    ${IfNot} ${RunningX64}
        MessageBox MB_OK|MB_ICONSTOP \
            "${APP} is 64-bit only and this is a 32-bit Windows installation." /SD IDOK
        Abort
    ${EndIf}
    ; The installer stub is 32-bit (it runs anywhere), so without this every
    ; HKLM write below would be redirected into Wow6432Node - out of sight of
    ; 64-bit Add/Remove Programs and of the lookup two lines down.
    SetRegView 64
    ; Shortcuts and the uninstall entry are machine-wide, matching where the
    ; files go.
    SetShellVarContext all

    ; Deliberately not InstallDirRegKey: that is read before .onInit runs, which
    ; is before SetRegView 64, so it would look in the wrong registry view.
    ; $INSTDIR still holds the InstallDir default unless /D= overrode it on the
    ; command line, and an explicit /D= must win over the previous location.
    ${If} $INSTDIR == "$PROGRAMFILES64\${APP}"
        ReadRegStr $0 HKLM "${UNINST_KEY}" "InstallLocation"
        ${If} $0 != ""
            StrCpy $INSTDIR $0
        ${EndIf}
    ${EndIf}
FunctionEnd

Function un.onInit
    SetRegView 64
    SetShellVarContext all
FunctionEnd

; -------------------------------------------------------------- sections ----

Section "!${APP}" SecApp
    SectionIn RO
    SetOutPath "$INSTDIR"

    ; Windows will not replace a running executable, and the File commands below
    ; would stop halfway through with a write error. Delete fails on a mapped
    ; image and succeeds quietly when there is nothing there, which makes it a
    ; plugin-free "is it still running?" test.
    ClearErrors
    Delete "$INSTDIR\quicksearch.exe"
    Delete "$INSTDIR\quicksearch-cli.exe"
    ${If} ${Errors}
        MessageBox MB_OK|MB_ICONSTOP \
            "${APP} is still running. Close it and start this installer again." /SD IDOK
        Abort
    ${EndIf}

    File "${STAGE}/quicksearch.exe"
    File "${STAGE}/quicksearch-cli.exe"
    File "${STAGE}/quicksearch.ico"
    File "${STAGE}/README.md"
    File "${STAGE}/LICENSE.txt"
    File "${STAGE}/config_example.toml"
    ; No config.toml is installed, for the same reason the .deb ships none: one
    ; sitting next to the binary is portable mode, and it would override the
    ; personal config of every account on the machine. The app writes
    ; %APPDATA%\quicksearch\config.toml on first run instead.

    WriteUninstaller "$INSTDIR\uninstall.exe"

    WriteRegStr HKLM "${UNINST_KEY}" "DisplayName"          "${APP}"
    WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion"       "${VERSION}"
    WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon"          "$INSTDIR\quicksearch.ico"
    WriteRegStr HKLM "${UNINST_KEY}" "Publisher"            "${PUBLISHER}"
    WriteRegStr HKLM "${UNINST_KEY}" "URLInfoAbout"         "${HOMEPAGE}"
    WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation"      "$INSTDIR"
    WriteRegStr HKLM "${UNINST_KEY}" "UninstallString"      '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
    WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
    WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
    ; Add/Remove Programs reads this as a DWORD of kilobytes, which is exactly
    ; what /S=0K returns.
    ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
    IntFmt $0 "0x%08X" $0
    WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" $0
SectionEnd

Section "Start Menu shortcut" SecStartMenu
    ; One shortcut, no program folder: a single-application folder is noise in
    ; the Windows 10/11 Start menu, and the uninstaller lives in Add/Remove
    ; Programs rather than next to it.
    CreateShortcut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\quicksearch.exe" "" "$INSTDIR\quicksearch.ico"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
    CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\quicksearch.exe" "" "$INSTDIR\quicksearch.ico"
SectionEnd

; There is deliberately no "add to PATH" section, tempting as one is for
; quicksearch-cli. Editing PATH means reading the machine value, appending and
; writing it back, and NSIS strings in the standard build are capped at
; NSIS_MAX_STRLEN (1024 characters, `makensis -HDRINFO`). ReadRegStr truncates
; silently at that cap, so on any machine with a long PATH the write-back would
; destroy the rest of it - a spectacular failure for an optional checkbox. The
; README tells people to add the directory themselves.

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
    !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} \
        "The desktop app and quicksearch-cli, the terminal search tool."
    !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} \
        "Add ${APP} to the Start menu for all users."
    !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} \
        "Add a ${APP} shortcut to the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; The installer is elevated and anything it starts inherits that. Handing the
; path to the already-running Explorer starts the app as the logged-on user
; instead, so a first run creates its config and index in the right profile.
Function LaunchAsUser
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\quicksearch.exe"'
FunctionEnd

; ----------------------------------------------------------- uninstaller ----

Section "Uninstall"
    ; Same running-process test as the install side. Stopping here leaves the
    ; installation intact rather than gutted.
    ClearErrors
    Delete "$INSTDIR\quicksearch.exe"
    Delete "$INSTDIR\quicksearch-cli.exe"
    ${If} ${Errors}
        MessageBox MB_OK|MB_ICONSTOP \
            "${APP} is still running. Close it and try again." /SD IDOK
        Abort
    ${EndIf}

    Delete "$INSTDIR\quicksearch.ico"
    Delete "$INSTDIR\README.md"
    Delete "$INSTDIR\LICENSE.txt"
    Delete "$INSTDIR\config_example.toml"
    Delete "$INSTDIR\uninstall.exe"
    Delete "$SMPROGRAMS\${APP}.lnk"
    Delete "$DESKTOP\${APP}.lnk"
    ; Plain RMDir, never /r: a portable-mode config.toml and the index beside it
    ; may be sitting in this directory, and neither is ours to delete. A
    ; directory holding anything the installer did not put there simply stays.
    RMDir "$INSTDIR"

    DeleteRegKey HKLM "${UNINST_KEY}"
    ; The per-user config in %APPDATA%\quicksearch and the index in
    ; %LOCALAPPDATA%\quicksearch are left alone, the way apt leaves ~/.config
    ; alone: reinstalling picks up where the last install left off, and nobody
    ; loses an index to an upgrade.
SectionEnd
