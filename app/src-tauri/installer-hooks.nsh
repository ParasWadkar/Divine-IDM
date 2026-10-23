; Native messaging registration, run as part of install and uninstall.
;
; Without this the app installs fine and the extension does nothing: a
; browser will not launch a native messaging host that is not registered
; under its own key in HKCU, so every download would fall back to the
; browser's own downloader with no visible error anywhere. Registration is
; therefore part of installing, not a thing to tell people to run afterwards.
;
; The work itself is `idm.exe --install-host`, which is the same path the
; developer build uses. The installer only supplies the arguments, so there
; is one implementation of the registry layout rather than two that drift.

; The extension id, pinned by the keypair in extension/keys/ and embedded in
; the built manifest. A Chromium browser checks it before it will start the
; host at all, which is why it is passed rather than left open.
;
; make-installer.ps1 fails the build if this disagrees with
; extension/keys/chromium.pub.txt, so regenerating the key cannot silently
; ship an installer that registers an id no browser will ever present.
!define IDM_EXTENSION_ID "epjaomgmiimpmbipmfbnloinhaehdaie"

!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Registering the browser connector..."

  ; --all-browsers writes the key for every browser in the table, not only
  ; the ones installed now. The alternative leaves someone who installs
  ; Firefox next month with an extension that silently does nothing and no
  ; way to fix it short of a command line, and the cost of the other
  ; direction is an unused registry key. Uninstall removes them all.
  nsExec::ExecToLog '"$INSTDIR\idm.exe" --install-host --all-browsers --extension-id ${IDM_EXTENSION_ID}'
  Pop $0

  ; Not fatal. A failed registration means the extension cannot reach the
  ; app, which is worth saying plainly, but the app itself is installed and
  ; works on its own, so rolling the install back would be the worse answer.
  ${If} $0 != 0
    DetailPrint "Could not register the browser connector (code $0)."
    DetailPrint "Run this afterwards to retry:"
    DetailPrint "  $INSTDIR\idm.exe --install-host --all-browsers --extension-id ${IDM_EXTENSION_ID}"
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Before the files go, because this needs idm.exe to still be there.
  ; Leaving the keys behind would point every browser at a path that no
  ; longer exists, and a browser that tries to start a missing host reports
  ; nothing the user can act on.
  DetailPrint "Removing the browser connector..."
  nsExec::ExecToLog '"$INSTDIR\idm.exe" --uninstall-host'
  Pop $0
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
