// armer.exe - keeps the TLS helper's guarded page armed from OUTSIDE the game process.
//
//   armer.exe --pid <game pid> --log <powder_tls.log / hook_guard.log> [--page <hex page address>]
//
// PAGE_GUARD is one-shot (a fault clears it), and VMProtect blocks the game's OWN VirtualProtect on its code,
// so re-arming must come from another process: this one re-arms with VirtualProtectEx, which VMProtect cannot
// block. The hook itself (the guard handler + the pinned-certificate check) lives in the injected DLL
// (hook_guard.dll on Windows, version.dll on Linux/Proton); this only keeps the guard set so each certificate
// check is trapped. It never patches the function.
//
// On Windows the launcher injects hook_guard.dll then runs this. On Linux the version.dll proxy launches this
// as a child (so it shares the game's wineserver) and passes --page (the page it already computed). Without
// --page the page is found from the game's module base. Either way it waits for "attach" in the log (the DLL
// installed its handler) before arming, re-arms tight until the first "hook hit", then eases off. Exit: 2 game
// gone, 1 error.
//
//   zig cc -target x86_64-windows-gnu -O2 -o bin/armer.exe armer.c
//   (or x86_64-w64-mingw32-gcc -O2 -o bin/armer.exe armer.c)

#include <windows.h>
#include <tlhelp32.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define TARGET_RVA 0x1D3E290ULL

static int log_has(const char *path, const char *needle) {
    FILE *f = fopen(path, "rb");
    if (!f) return 0;
    static char buf[65536];
    size_t n = fread(buf, 1, sizeof(buf) - 1, f); buf[n] = 0; fclose(f);
    return strstr(buf, needle) != NULL;
}

static int alive(HANDLE proc) {
    DWORD ec;
    return GetExitCodeProcess(proc, &ec) && ec == STILL_ACTIVE;
}

static uintptr_t module_base(DWORD pid) {
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
    if (snap == INVALID_HANDLE_VALUE) return 0;
    MODULEENTRY32 me; me.dwSize = sizeof(me);
    uintptr_t base = 0;
    if (Module32First(snap, &me)) base = (uintptr_t)me.modBaseAddr;   // first module = the exe
    CloseHandle(snap);
    return base;
}

int main(int argc, char **argv) {
    DWORD pid = 0;
    const char *log = NULL;
    uintptr_t page = 0;
    for (int i = 1; i + 1 < argc; i++) {
        if (!strcmp(argv[i], "--pid")) pid = (DWORD)strtoul(argv[++i], NULL, 10);
        else if (!strcmp(argv[i], "--log")) log = argv[++i];
        else if (!strcmp(argv[i], "--page")) page = (uintptr_t)strtoull(argv[++i], NULL, 0);
    }
    if (!pid || !log) { printf("usage: armer.exe --pid <pid> --log <log> [--page <hex>]\n"); return 1; }
    HANDLE proc = OpenProcess(PROCESS_VM_OPERATION | PROCESS_QUERY_INFORMATION, FALSE, pid);
    if (!proc) { printf("armer: OpenProcess(%lu) failed %lu\n", pid, GetLastError()); return 1; }

    // the DLL must have installed its handler before the guard is armed, or a guard fault would crash the game
    while (!log_has(log, "attach")) { if (!alive(proc)) return 2; Sleep(100); }
    if (!page) {
        uintptr_t base = 0;
        while (!(base = module_base(pid))) { if (!alive(proc)) return 2; Sleep(200); }
        SYSTEM_INFO si; GetSystemInfo(&si);
        page = (base + TARGET_RVA) & ~((uintptr_t)si.dwPageSize - 1);
    }
    void *addr = (void *)page;
    printf("armer: pid %lu page %p, re-arming for the session\n", pid, addr); fflush(stdout);

    unsigned c = 0;
    int warm = 1;
    for (;;) {
        DWORD old;
        VirtualProtectEx(proc, addr, 0x1000, PAGE_EXECUTE_READ | PAGE_GUARD, &old);
        if (warm) {
            if ((++c & 0x3FF) == 0) {
                if (log_has(log, "hook hit")) { warm = 0; printf("armer: first handshake trapped\n"); fflush(stdout); }
                if (!alive(proc)) { printf("armer: game exited\n"); return 2; }
            }
        } else {
            Sleep(5);
            if (!alive(proc)) { printf("armer: game exited\n"); return 2; }
        }
    }
}
