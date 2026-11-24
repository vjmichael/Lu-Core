; Lu installer (NSIS 3). Built by scripts/build-installer.ps1, which passes:
;   /DSTAGE=<folder with lu.exe and bundled\>   /DVERSION=<x.y.z>   /DOUTFILE=<setup.exe>
;
; Per-user install (no administrator rights): %LOCALAPPDATA%\Programs\Lu.
; Lu keeps its data (conversations, models list, engines) in %LOCALAPPDATA%\Lu,
; separate from development builds (Lu-Dev). On first start Lu sets up the
; bundled engines from bundled\ with the same checks as a download.

Unicode true
SetCompressor /SOLID lzma
SetCompressorDictSize 64
RequestExecutionLevel user
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "0.1.0"
!endif

Name "Lu"
Caption "Lu ${VERSION} Setup"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\Lu"
InstallDirRegKey HKCU "Software\Lu" "InstallDir"
BrandingText "Lu ${VERSION}"
Icon "..\crates\lu-desktop\icons\icon.ico"
UninstallIcon "..\crates\lu-desktop\icons\icon.ico"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!define MUI_ICON "..\crates\lu-desktop\icons\icon.ico"
!define MUI_UNICON "..\crates\lu-desktop\icons\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\lu.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Lu"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "Lu"
VIAddVersionKey "FileDescription" "Lu Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Personal build"

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Lu"

Function .onInit
  ; Lu's windows run on Microsoft Edge WebView2 (part of Windows 11; most
  ; Windows 10 PCs have it).
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ReadRegStr $1 HKCU "Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
  ${If} $0 == ""
  ${AndIf} $1 == ""
    MessageBox MB_OKCANCEL|MB_ICONEXCLAMATION "Lu needs Microsoft Edge WebView2, which was not found on this PC.$\n$\nInstall it from https://developer.microsoft.com/microsoft-edge/webview2/ and run this setup again, or press OK to install Lu anyway." IDOK +2
    Abort
  ${EndIf}
FunctionEnd

Section "Lu" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${STAGE}\lu.exe"
  File "${STAGE}\THIRD_PARTY.txt"
  SetOutPath "$INSTDIR\bundled"
  File /r "${STAGE}\bundled\*.*"
  SetOutPath "$INSTDIR"

  CreateDirectory "$SMPROGRAMS\Lu"
  CreateShortcut "$SMPROGRAMS\Lu\Lu.lnk" "$INSTDIR\lu.exe"
  CreateShortcut "$DESKTOP\Lu.lnk" "$INSTDIR\lu.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "Software\Lu" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "Lu"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "Lu"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\lu.exe"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\lu.exe"
  Delete "$INSTDIR\THIRD_PARTY.txt"
  Delete "$INSTDIR\uninstall.exe"
  RMDir /r "$INSTDIR\bundled"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Lu\Lu.lnk"
  RMDir "$SMPROGRAMS\Lu"
  Delete "$DESKTOP\Lu.lnk"
  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "Software\Lu"
  ; Conversations, settings and installed engines stay unless the user says otherwise.
  MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "Also delete Lu's data in $LOCALAPPDATA\Lu: your conversations, settings, installed engines and the models Lu downloaded there?$\n$\nModels kept elsewhere (for example in LM Studio's folder) are not touched." IDNO keep
    RMDir /r "$LOCALAPPDATA\Lu"
  keep:
SectionEnd
