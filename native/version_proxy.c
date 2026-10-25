// version.dll proxy - the Linux/Proton way to load Powder's TLS helper into Steep.
//
// On Linux the game runs under Wine/Proton, and the Windows launcher's injection (CreateRemoteThread) is not
// available. But Steep statically imports version.dll (one function, GetFileVersionInfoA), so Wine loads a
// version.dll placed next to the game before the game's code runs. This file is that version.dll: it forwards
// every version.dll export to vers_powder.dll (a copy of the real one, placed beside it - see version.def),
// and in DllMain it starts the same certificate hook as hook_guard.c, armed from inside this process.
//
// VMProtect unpacks ssl_verify_cert_chain after the imported DLLs load, so a worker thread waits until the
// function's prologue is in place, then arms the guarded page and keeps it armed. The hook never changes code
// bytes (it only reads the certificate and returns a result from the guard handler), so VMProtect's integrity
// check, which reads the unchanged bytes, still passes. The check and the guard handler are tls_hook.h, shared
// with the Windows build.
//
// powder-wrap places this as <game>/version.dll with vers_powder.dll beside it, writes version.pin (the pinned
// certificate), sets WINEDLLOVERRIDES=version=n,b, and removes them on exit. Logs to version.log next to it.
//
//   x86_64-w64-mingw32-gcc -shared -O2 -o bin/version.dll version_proxy.c version.def -lbcrypt
//   (or zig cc -target x86_64-windows-gnu -shared -O2 -o bin/version.dll version_proxy.c version.def -lbcrypt)

#include "tls_hook.h"

#define WAIT_UNPACK_MS 120000          // the protector may take a while to unpack on a slow disk
#define REARM_SLEEP_MS 5               // between re-arms once the first handshake is trapped

static DWORD WINAPI worker(LPVOID p) {
    (void)p;
    // wait until VMProtect has unpacked ssl_verify_cert_chain (its prologue is in place)
    for (int waited = 0; !tls_unpacked(); waited += 50) {
        if (waited >= WAIT_UNPACK_MS) {
            tls_log("ssl_verify_cert_chain never unpacked (wrong build, or still loading): not hooking");
            return 0;
        }
        Sleep(50);
    }
    if (!tls_arm()) {
        tls_log("could not arm the guard page (VirtualProtect blocked?): not hooking");
        return 0;
    }
    tls_log("armed: target=%p pid=%lu pin=%s", (void *)g_target, GetCurrentProcessId(),
            g_have_pin ? "yes" : "MISSING");
    // The guard is one-shot: any access to this 4 KiB code page clears it. Re-arm as fast as possible until
    // the first handshake is trapped (so the guard is set whenever ssl_verify_cert_chain runs), then back off.
    // g_faults climbing means the handler fires (so the hook works); staying 0 means guard faults never reach
    // our VEH under Wine, and a different hook is needed. Status is logged for the first seconds either way.
    DWORD last = GetTickCount();
    for (;;) {
        tls_arm();
        DWORD now = GetTickCount();
        if (now - last >= 2000) {
            MEMORY_BASIC_INFORMATION mbi;
            int guarded = VirtualQuery((void *)g_page, &mbi, sizeof(mbi)) && (mbi.Protect & PAGE_GUARD);
            tls_log("status: guard faults=%ld, hook hits=%ld, guard currently %s", g_faults, g_hits,
                    guarded ? "set" : "cleared");
            last = now;
        }
        if (g_hits > 0) Sleep(REARM_SLEEP_MS);    // caught at least one: ease off the CPU
    }
    return 0;
}

BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, LPVOID r) {
    (void)r;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    DisableThreadLibraryCalls(h);
    wchar_t self[MAX_PATH], pin[MAX_PATH];
    DWORD n = GetModuleFileNameW(h, self, MAX_PATH);
    if (n < 1 || n >= MAX_PATH) return TRUE;
    // fixed names beside this DLL (the module name's case varies on a case-sensitive host filesystem)
    tls_sibling(self, L"powder_tls.log", g_log);
    tls_sibling(self, L"powder_tls.pin", pin);
    tls_load_pin_at(pin);
    tls_init_target();
    AddVectoredExceptionHandler(1, tls_veh);
    tls_log("version.dll proxy loaded pid=%lu; waiting for the game code to unpack", GetCurrentProcessId());
    CreateThread(NULL, 0, worker, NULL, 0, NULL);
    return TRUE;
}
