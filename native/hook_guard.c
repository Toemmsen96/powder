// hook_guard.dll - lets Steep accept the Powder server's own TLS certificate, and only that (Windows play build).
//
// Steep verifies certificates with a statically linked OpenSSL 1.0.2n (ssl_verify_cert_chain, in Ubisoft's
// UbiServices SDK), and the exe is protected by VMProtect, which detects code patches and debug registers. So
// this DLL changes no code bytes: it traps execution of ssl_verify_cert_chain with a guarded memory page and a
// vectored exception handler. PAGE_GUARD is one-shot and VMProtect blocks in-process VirtualProtect on its
// ranges, so armer.exe re-arms the page from outside for the whole session. The hook itself (the guard handler
// and the pinned-certificate check) is tls_hook.h, shared with the Linux proxy (version_proxy.c).
//
// On each trapped call the handler accepts the chain only when the server certificate is byte-for-byte the one
// the launcher pinned (hook_guard.pin, written next to this DLL before injecting); anything else is rejected
// as without the DLL. It fails closed: no pin, an unreadable certificate, or an unexpected layout all reject.
//
// Injected by the launcher (LoadLibraryW via CreateRemoteThread). Logs to hook_guard.log next to this DLL;
// armer.exe and the launcher read "attach" and "hook hit" from it. Only valid for the Steep.exe build the
// launcher checks before injecting (see tls_hook.h and powder/launcher/game.py).
//
//   zig cc -target x86_64-windows-gnu -shared -O2 -o bin/hook_guard.dll hook_guard.c -lbcrypt
//   (or x86_64-w64-mingw32-gcc -shared -O2 -o bin/hook_guard.dll hook_guard.c -lbcrypt)

#include "tls_hook.h"

BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, LPVOID r) {
    (void)r;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    DisableThreadLibraryCalls(h);
    wchar_t self[MAX_PATH];
    DWORD n = GetModuleFileNameW(h, self, MAX_PATH);           // this DLL's path; .dll -> .pin and .log
    if (n < 4 || n >= MAX_PATH) return TRUE;
    tls_load_pin(self, n - 4);
    wcscpy(g_log, self);
    wcscpy(g_log + n - 4, L".log");
    tls_init_target();
    AddVectoredExceptionHandler(1, tls_veh);
    tls_arm();
    tls_log("attach: target=%p pid=%lu pin=%s", (void *)g_target, GetCurrentProcessId(),
            g_have_pin ? "yes" : "MISSING");
    return TRUE;
}
