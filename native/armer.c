// armer.exe - keeps hook_guard.dll's guarded page armed so every TLS handshake is trapped (play build).
//
//   armer.exe --pid <Steep pid> --log <path to hook_guard.log>
//
// PAGE_GUARD is one-shot (the fault clears it), and VMProtect blocks the game's own VirtualProtect on its
// code, so armer re-arms the page from outside with VirtualProtectEx. Unlike the earlier helper it never
// patches the function (the earlier one wrote `mov eax,1; ret` to accept every certificate): the check now
// lives in hook_guard.dll's handler and must run on each handshake, so the page stays armed for the whole
// session. Certificate verifications are infrequent, so a short sleep between re-arms keeps the CPU idle;
// a handshake landing in that gap just runs the game's own check (it fails and the game retries, when the
// guard is set again). Exit codes: 2 game exited, 1 error (it otherwise runs until the game closes).
//
//   zig cc -target x86_64-windows-gnu -O2 -o bin/armer.exe armer.c

#include <windows.h>
#include <tlhelp32.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define TARGET_RVA 0x1D3E290ULL

static const char *g_log = NULL;

static int log_has(const char *needle) {
    FILE *f = fopen(g_log, "rb");
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
    for (int i = 1; i + 1 < argc; i++) {
        if (!strcmp(argv[i], "--pid")) pid = (DWORD)strtoul(argv[++i], NULL, 10);
        else if (!strcmp(argv[i], "--log")) g_log = argv[++i];
    }
    if (!pid || !g_log) { printf("usage: armer.exe --pid <pid> --log <hook_guard.log>\n"); return 1; }
    HANDLE proc = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_QUERY_INFORMATION, FALSE, pid);
    if (!proc) { printf("armer: OpenProcess(%lu) failed %lu\n", pid, GetLastError()); return 1; }

    // the VEH must exist before the guard is armed, or a guard fault would crash the game
    while (!log_has("attach")) { if (!alive(proc)) return 2; Sleep(150); }
    uintptr_t base = 0;
    while (!(base = module_base(pid))) { if (!alive(proc)) return 2; Sleep(200); }
    void *addr = (void *)(base + TARGET_RVA);
    printf("armer: pid %lu target %p, re-arming for the session\n", pid, addr); fflush(stdout);

    // Re-arm continuously. A tight loop until the first handshake (so the page is surely set while the game
    // reaches its first TLS connection), then a short sleep between passes (handshakes are rare after that).
    unsigned c = 0;
    int warm = 1;
    for (;;) {
        DWORD old;
        VirtualProtectEx(proc, addr, 0x1000, PAGE_EXECUTE_READ | PAGE_GUARD, &old);
        if (warm) {
            if ((++c & 0x7FF) == 0) {
                if (log_has("hook hit")) { warm = 0; printf("armer: first handshake trapped\n"); fflush(stdout); }
                if (!alive(proc)) { printf("armer: game exited\n"); return 2; }
            }
        } else {
            Sleep(5);
            if (!alive(proc)) { printf("armer: game exited\n"); return 2; }
        }
    }
}
