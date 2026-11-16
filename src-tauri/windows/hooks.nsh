; NSIS installer hooks (bundle > windows > nsis > installerHooks).
;
; Until 0.3 the app was called "EverlookDB Uploader". NSIS keys the install folder, the uninstall entry and the
; shortcuts by product name, so "EverlookDB" installs next to it in a new folder. Before installing, this finds the
; old install, runs its own uninstaller silently and then puts the player's choices back on the new copy:
;   - the old uninstaller stops the old app if it is running, removes its exe, its Start menu and desktop shortcuts,
;     its uninstall entry and its login item (Run value);
;   - run silently it never deletes the app's data: that needs the "delete app data" checkbox, which a silent
;     uninstall never shows (its state stays 0), so %APPDATA%\com.everlookdb.uploader (settings, addon records,
;     kept zips) and %LOCALAPPDATA%\com.everlookdb.uploader survive; the identifier is unchanged, so the new app
;     reads them as before;
;   - afterwards the login item is written again for the new exe (same name, the app's autostart uses it), and a
;     Start menu / desktop shortcut is made for the new app where the old one had one (an update installs with
;     /UPDATE, which makes no shortcuts by itself).
;
; The EDB_OLD_* names can be defined before this file is included, so a test build can migrate a fake old install
; instead of the real one.

!ifndef EDB_OLD_PRODUCT
  !define EDB_OLD_PRODUCT "EverlookDB Uploader"
!endif
!ifndef EDB_OLD_BINARY
  !define EDB_OLD_BINARY "everlookdb-uploader.exe"
!endif
; the login item's name: tauri-plugin-autostart's app_name in main.rs (kept from the Uploader)
!ifndef EDB_LOGIN_ITEM
  !define EDB_LOGIN_ITEM "EverlookDB Uploader"
!endif
!define EDB_OLD_UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${EDB_OLD_PRODUCT}"
!define EDB_RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"

Var EdbOldDir
Var EdbOldRun
Var EdbOldStartMenu
Var EdbOldDesktop
Var EdbMigrated

!macro EDB_UNQUOTE var
  StrCpy $R9 ${var} 1
  ${If} $R9 == '"'
    StrCpy ${var} ${var} -1 1
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  StrCpy $EdbMigrated 0
  StrCpy $EdbOldRun ""
  StrCpy $EdbOldStartMenu 0
  StrCpy $EdbOldDesktop 0

  ; where the old app is: its uninstall entry, else the folder its installer remembered, else its default folder
  ReadRegStr $EdbOldDir HKCU "${EDB_OLD_UNINSTKEY}" "InstallLocation"
  !insertmacro EDB_UNQUOTE $EdbOldDir
  ${If} $EdbOldDir == ""
    ReadRegStr $EdbOldDir HKCU "Software\${MANUFACTURER}\${EDB_OLD_PRODUCT}" ""
  ${EndIf}
  ${If} $EdbOldDir == ""
    StrCpy $EdbOldDir "$LOCALAPPDATA\${EDB_OLD_PRODUCT}"
  ${EndIf}

  ${If} ${FileExists} "$EdbOldDir\${EDB_OLD_BINARY}"
    DetailPrint "Moving ${EDB_OLD_PRODUCT} ($EdbOldDir) over to ${PRODUCTNAME}"
    ReadRegStr $EdbOldRun HKCU "${EDB_RUNKEY}" "${EDB_LOGIN_ITEM}"
    !insertmacro IsShortcutTarget "$SMPROGRAMS\${EDB_OLD_PRODUCT}.lnk" "$EdbOldDir\${EDB_OLD_BINARY}"
    Pop $EdbOldStartMenu
    !insertmacro IsShortcutTarget "$DESKTOP\${EDB_OLD_PRODUCT}.lnk" "$EdbOldDir\${EDB_OLD_BINARY}"
    Pop $EdbOldDesktop

    ${If} ${FileExists} "$EdbOldDir\uninstall.exe"
      ; in place (_?=) so ExecWait waits for it; it can't delete itself then, done below
      ClearErrors
      ExecWait '"$EdbOldDir\uninstall.exe" /S _?=$EdbOldDir' $0
      ${If} ${Errors}
        DetailPrint "Couldn't run the ${EDB_OLD_PRODUCT} uninstaller"
      ${EndIf}
    ${Else}
      ; no uninstaller: stop it and remove the exe and its uninstall entry by hand
      !insertmacro CheckIfAppIsRunning "$EdbOldDir\${EDB_OLD_BINARY}" "${EDB_OLD_PRODUCT}"
      Delete "$EdbOldDir\${EDB_OLD_BINARY}"
      ${IfNot} ${FileExists} "$EdbOldDir\${EDB_OLD_BINARY}"
        DeleteRegKey HKCU "${EDB_OLD_UNINSTKEY}"
        DeleteRegValue HKCU "${EDB_RUNKEY}" "${EDB_LOGIN_ITEM}"
      ${EndIf}
    ${EndIf}

    ${If} ${FileExists} "$EdbOldDir\${EDB_OLD_BINARY}"
      DetailPrint "${EDB_OLD_PRODUCT} is still installed; leaving it as it is"
    ${Else}
      StrCpy $EdbMigrated 1
      Delete "$EdbOldDir\uninstall.exe"
      ${If} $EdbOldDir != $INSTDIR
        RMDir "$EdbOldDir"
      ${EndIf}
      ; shortcuts the uninstaller left (it only removes ones pointing at its exe, which these do)
      ${If} $EdbOldStartMenu = 1
        Delete "$SMPROGRAMS\${EDB_OLD_PRODUCT}.lnk"
      ${EndIf}
      ${If} $EdbOldDesktop = 1
        Delete "$DESKTOP\${EDB_OLD_PRODUCT}.lnk"
      ${EndIf}
      ; the old installer's remembered folder and language
      DeleteRegKey HKCU "Software\${MANUFACTURER}\${EDB_OLD_PRODUCT}"
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ${If} $EdbMigrated = 1
    ${If} $EdbOldRun != ""
      ; the format tauri-plugin-autostart writes, so the app sees it as its own login item
      WriteRegStr HKCU "${EDB_RUNKEY}" "${EDB_LOGIN_ITEM}" "$INSTDIR\${MAINBINARYNAME}.exe --background"
    ${EndIf}
    ${If} $EdbOldStartMenu = 1
    ${AndIfNot} ${FileExists} "$SMPROGRAMS\${PRODUCTNAME}.lnk"
      CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
      !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    ${EndIf}
    ${If} $EdbOldDesktop = 1
    ${AndIfNot} ${FileExists} "$DESKTOP\${PRODUCTNAME}.lnk"
      CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
      !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; the template removes a login item named after the product; ours keeps the Uploader's name
  ${If} $UpdateMode <> 1
    ReadRegStr $R9 HKCU "${EDB_RUNKEY}" "${EDB_LOGIN_ITEM}"
    StrCpy $R9 $R9 -13
    ${If} $R9 == "$INSTDIR\${MAINBINARYNAME}.exe"
      DeleteRegValue HKCU "${EDB_RUNKEY}" "${EDB_LOGIN_ITEM}"
    ${EndIf}
  ${EndIf}
!macroend
