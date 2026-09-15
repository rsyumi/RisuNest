!macro NSIS_HOOK_PREINSTALL
  StrCpy $R7 ""
  IfFileExists "$INSTDIR\risunest-sync-manager.exe" 0 sync_prepare_done
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" installer prepare'
  Pop $0
  Pop $1
  StrCmp $0 "0" 0 sync_prepare_failed
  StrCpy $R7 $1 64
  StrLen $2 $R7
  StrCmp $2 "64" sync_prepare_done sync_prepare_failed
  sync_prepare_failed:
  DetailPrint "Existing RisuNest Sync could not be stopped. Server data was preserved."
  IfSilent sync_prepare_quiet sync_prepare_interactive
  sync_prepare_interactive:
  MessageBox MB_OK|MB_ICONSTOP "Stop the existing RisuNest sync server before updating. No server data has been removed."
  sync_prepare_quiet:
  SetErrorLevel 1
  Abort
  sync_prepare_done:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  StrCpy $R6 "0"
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" update schedule reconcile lock-held'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_register_startup
  DetailPrint "Scheduled updates could not be registered."
  Goto sync_install_failed
  sync_register_startup:
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" autostart install lock-held'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_start
  DetailPrint "Automatic startup could not be registered."
  Goto sync_install_failed
  sync_start:
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" installer start-and-verify lock-held'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_release_guard
  DetailPrint "RisuNest Sync was installed, but its server could not start."
  Goto sync_install_failed
  sync_install_failed:
  StrCpy $R6 "1"
  SetErrorLevel 1
  IfSilent sync_release_guard 0
  MessageBox MB_OK|MB_ICONEXCLAMATION "RisuNest Sync was installed, but the server could not start. Open the app and check the server status."
  sync_release_guard:
  StrCmp $R7 "" sync_install_done
  StrCmp $R6 "1" sync_cancel_guard
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" installer finish "$R7"'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_install_done
  DetailPrint "The installer update lock could not be released."
  SetErrorLevel 1
  Goto sync_install_done
  sync_cancel_guard:
  CreateDirectory "$LOCALAPPDATA\RisuNestSync\manager-update"
  FileOpen $0 "$LOCALAPPDATA\RisuNestSync\manager-update\installer-$R7.cancel" w
  IfErrors sync_cancel_guard_failed
  FileWrite $0 '{"cancel":true}'
  FileClose $0
  Goto sync_install_done
  sync_cancel_guard_failed:
  DetailPrint "The installer update guard could not restore the server."
  SetErrorLevel 1
  sync_install_done:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  StrCpy $R7 ""
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" installer prepare'
  Pop $0
  Pop $1
  StrCmp $0 "0" 0 sync_uninstall_failed
  StrCpy $R7 $1 64
  StrLen $2 $R7
  StrCmp $2 "64" 0 sync_uninstall_failed
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" uninstall lock-held'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_uninstall_done
  sync_uninstall_failed:
  StrCmp $R7 "" sync_uninstall_cancel_done
  CreateDirectory "$LOCALAPPDATA\RisuNestSync\manager-update"
  FileOpen $0 "$LOCALAPPDATA\RisuNestSync\manager-update\installer-$R7.cancel" w
  IfErrors sync_uninstall_cancel_done
  FileWrite $0 '{"cancel":true}'
  FileClose $0
  sync_uninstall_cancel_done:
  DetailPrint "Server shutdown or startup removal failed. Server data was preserved."
  IfSilent sync_uninstall_quiet sync_uninstall_interactive
  sync_uninstall_interactive:
  MessageBox MB_OK|MB_ICONSTOP "Server shutdown or startup removal failed. Close the server and retry. Server data will be preserved."
  sync_uninstall_quiet:
  SetErrorLevel 1
  Abort
  sync_uninstall_done:
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  StrCmp $R7 "" sync_uninstall_guard_done
  CreateDirectory "$LOCALAPPDATA\RisuNestSync\manager-update"
  FileOpen $0 "$LOCALAPPDATA\RisuNestSync\manager-update\installer-$R7.release" w
  IfErrors sync_uninstall_guard_failed
  FileWrite $0 '{"release":true}'
  FileClose $0
  Goto sync_uninstall_guard_done
  sync_uninstall_guard_failed:
  DetailPrint "The installer update lock could not be released."
  SetErrorLevel 1
  sync_uninstall_guard_done:
!macroend
