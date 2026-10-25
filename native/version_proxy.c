// version.dll proxy - the Linux/Proton way to load Powder's TLS helper into Steep.
//
// On Linux the game runs under Wine/Proton, and the Windows launcher's injection (CreateRemoteThread) is not
// available. But Steep statically imports version.dll (GetFileVersionInfoA), so Wine loads a version.dll placed
// next to the game before the game's code runs. This file is that version.dll: it forwards every version.dll
// export to vers_powder.dll (a copy of the real one, see version.def) and, in DllMain, installs the same
// certificate hook as hook_guard.c (guarded page + VEH, the pinned-tbsCertificate check in tls_hook.h).
//
// VMProtect blocks the game's OWN VirtualProtect on its code, so the guard cannot be re-armed from inside the
// game (seen live: it arms once, then stays cleared). So, exactly as on Windows, the guard is re-armed from
// another process: once the code is unpacked, a worker launches armer.exe (deployed beside this DLL) as a
// child - same wineserver, so it can VirtualProtectEx the game, which VMProtect cannot block. The hook never
// changes code bytes, so VMProtect's integrity check still passes.
//
// powder-wrap places this as <game>/version.dll with vers_powder.dll and armer.exe beside it, writes
// powder_tls.pin (the pinned certificate), sets the version.dll override, and removes them on exit. Logs to
// powder_tls.log next to this DLL.
//
//   x86_64-w64-mingw32-gcc -shared -O2 -o bin/version.dll version_proxy.c version.def -lbcrypt

#include "tls_hook.h"

#define WAIT_UNPACK_MS 120000          // the protector may take a while to unpack on a slow disk

static wchar_t g_dir[MAX_PATH];        // this DLL's directory (with trailing backslash), for armer.exe

// Launch armer.exe (beside this DLL) to re-arm the guard from outside this process.
static void launch_armer(void) {
    wchar_t exe[MAX_PATH], cmd[MAX_PATH * 2];
    _snwprintf(exe, MAX_PATH, L"%sarmer.exe", g_dir);
    _snwprintf(cmd, MAX_PATH * 2, L"\"%s\" --pid %lu --page 0x%llx --log \"%s\"",
               exe, GetCurrentProcessId(), (unsigned long long)g_page, g_log);
    STARTUPINFOW si; ZeroMemory(&si, sizeof(si)); si.cb = sizeof(si);
    PROCESS_INFORMATION pi;
    if (CreateProcessW(exe, cmd, NULL, NULL, FALSE, CREATE_NO_WINDOW, NULL, g_dir, &si, &pi)) {
        tls_log("launched armer.exe (external re-arming)");
        CloseHandle(pi.hThread); CloseHandle(pi.hProcess);
    } else {
        tls_log("could not launch armer.exe (%lu): the guard cannot be kept armed", GetLastError());
    }
}

static DWORD WINAPI worker(LPVOID p) {
    (void)p;
    for (int waited = 0; !tls_unpacked(); waited += 50) {
        if (waited >= WAIT_UNPACK_MS) {
            tls_log("ssl_verify_cert_chain never unpacked (wrong build, or still loading): not hooking");
            return 0;
        }
        Sleep(50);
    }
    // the handler is installed (DllMain): tell armer it may arm, and launch it
    tls_log("attach: target=%p pid=%lu pin=%s", (void *)g_target, GetCurrentProcessId(), g_have_pin ? "yes" : "MISSING");
    launch_armer();
    DWORD last = GetTickCount();
    for (;;) {
        Sleep(500);
        DWORD now = GetTickCount();
        if (now - last >= 2000) {
            MEMORY_BASIC_INFORMATION mbi;
            int guarded = VirtualQuery((void *)g_page, &mbi, sizeof(mbi)) && (mbi.Protect & PAGE_GUARD);
            tls_log("status: guard faults=%ld, hook hits=%ld, guard currently %s", g_faults, g_hits,
                    guarded ? "set" : "cleared");
            last = now;
        }
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
    wcscpy(g_dir, self);
    wchar_t *slash = wcsrchr(g_dir, L'\\');
    if (slash) slash[1] = 0; else g_dir[0] = 0;          // "<dir>\"
    tls_load_pin_at(pin);
    tls_init_target();
    AddVectoredExceptionHandler(1, tls_veh);
    tls_log("version.dll proxy loaded pid=%lu; waiting for the game code to unpack", GetCurrentProcessId());
    CreateThread(NULL, 0, worker, NULL, 0, NULL);
    return TRUE;
}
