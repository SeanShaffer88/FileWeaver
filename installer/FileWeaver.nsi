; FileWeaver — per-user NSIS installer.
;
; Built by .github/workflows/release.yml with:
;   makensis /DAppVersion=<x.y.z> installer\FileWeaver.nsi
; (Paths are relative to this script's folder, which is makensis' working
; directory while compiling.)
;
; Per-user install: no administrator rights, no services, no shell
; extensions, no autostart entries. The operation ledger lives in the
; "data" folder next to the executable; install and uninstall never touch
; it. `fileweaver --clean` removes that data on request.

Unicode true

!ifndef AppVersion
  !define AppVersion "0.2.0"
!endif

Name "FileWeaver"
Caption "FileWeaver ${AppVersion} Setup"
OutFile "..\dist\FileWeaver-Setup.exe"
InstallDir "$LOCALAPPDATA\Programs\FileWeaver"
InstallDirRegKey HKCU "Software\FileWeaver" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

VIProductVersion "${AppVersion}.0"
VIAddVersionKey "ProductName" "FileWeaver"
VIAddVersionKey "FileDescription" "FileWeaver Setup"
VIAddVersionKey "FileVersion" "${AppVersion}"
VIAddVersionKey "ProductVersion" "${AppVersion}"
VIAddVersionKey "LegalCopyright" "MIT License"

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "$INSTDIR"
  File "..\target\release\fileweaver.exe"

  WriteRegStr HKCU "Software\FileWeaver" "InstallDir" "$INSTDIR"

  ; Start menu shortcut.
  CreateDirectory "$SMPROGRAMS\FileWeaver"
  CreateShortcut "$SMPROGRAMS\FileWeaver\FileWeaver.lnk" "$INSTDIR\fileweaver.exe"

  ; Uninstaller and its Add/Remove Programs registration.
  WriteUninstaller "$INSTDIR\Uninstall.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "DisplayName" "FileWeaver"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "DisplayVersion" "${AppVersion}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "Publisher" "FileWeaver contributors"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "DisplayIcon" "$INSTDIR\fileweaver.exe"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\fileweaver.exe"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\FileWeaver\FileWeaver.lnk"
  RMDir "$SMPROGRAMS\FileWeaver"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\FileWeaver"
  DeleteRegKey HKCU "Software\FileWeaver"
  ; $INSTDIR\data holds the user's operation ledger and rollback history:
  ; it is deliberately NOT removed (run `fileweaver --clean` first, or
  ; delete the folder manually, to erase it). RMDir therefore only
  ; succeeds when nothing but program files was ever there.
  RMDir "$INSTDIR"
SectionEnd
