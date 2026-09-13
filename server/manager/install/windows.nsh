!macro NSIS_HOOK_PREINSTALL
  IfFileExists "$INSTDIR\risunest-sync-manager.exe" 0 sync_prepare_done
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" prepare-update'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_prepare_done
  MessageBox MB_OK|MB_ICONSTOP "Stop the existing RisuNest sync server before updating. No server data has been removed."
  Abort
  sync_prepare_done:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" autostart install'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_start
  MessageBox MB_OK|MB_ICONEXCLAMATION "RisuNest Sync was installed, but automatic startup could not be registered. Open Run Settings in the app to retry."
  Goto sync_install_done
  sync_start:
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" start'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_install_done
  MessageBox MB_OK|MB_ICONEXCLAMATION "RisuNest Sync was installed, but the server could not start. Open the app and check the server status."
  sync_install_done:
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToStack '"$INSTDIR\risunest-sync-manager.exe" uninstall'
  Pop $0
  Pop $1
  StrCmp $0 "0" sync_uninstall_done
  MessageBox MB_OK|MB_ICONSTOP "Server shutdown or startup removal failed. Close the server and retry. Server data will be preserved."
  Abort
  sync_uninstall_done:
!macroend
