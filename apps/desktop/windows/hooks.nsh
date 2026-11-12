; Ferrocast's NSIS installer hooks (Tauri's `installerHooks`).

; Uninstalling takes Ferrocast Camera away from apps: its DirectShow registration for this user
; (written the first time the camera started), in the 64-bit and the 32-bit registry views. The
; camera for every app (Windows 11), set up once as administrator, can only be shown while
; Ferrocast makes it, so nothing of it stays visible; Settings removes it entirely.
!macro NSIS_HOOK_POSTUNINSTALL
  SetRegView 64
  DeleteRegKey HKCU "Software\Classes\CLSID\{860BB310-5D01-11D0-BD3B-00A0C911CE86}\Instance\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}"
  DeleteRegKey HKCU "Software\Classes\CLSID\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}"
  SetRegView 32
  DeleteRegKey HKCU "Software\Classes\CLSID\{860BB310-5D01-11D0-BD3B-00A0C911CE86}\Instance\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}"
  DeleteRegKey HKCU "Software\Classes\CLSID\{9E5CC468-3A73-45C7-AE12-0E584A9A4E36}"
  SetRegView default
  ; Camera DLLs of older versions left by updates (an app had them open then).
  Delete "$INSTDIR\ferrocast_camera-*.dll"
  Delete "$INSTDIR\ferrocast_camera32-*.dll"
!macroend
