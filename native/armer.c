// armer.exe - arms or patches the TLS helper's hook from OUTSIDE the game process.
//
//   armer.exe --pid <game pid> --log <log> [--page <hex>]                       (guard mode, Windows)
//   armer.exe --pid <game pid> --log <log> --target <hex> --patchfn <hex> [--delayms N]  (patch mode, Linux)
//
// VMProtect blocks the game's OWN VirtualProtect/WriteProcessMemory on its code, so arming or patching must
// come from another process; this one does it with VirtualProtectEx / WriteProcessMemory, which VMProtect
// cannot block. The hook logic (the pinned-certificate check) lives in the injected DLL.
//
// Guard mode (Windows): keeps a one-shot PAGE_GUARD re-armed so the DLL's exception handler traps each call.
// Patch mode (Linux/Proton): the guard cannot be kept armed tightly enough through the wineserver, so instead
// write a 14-byte `jmp [rip+0]; <patchfn>` over ssl_verify_cert_chain's first bytes once, redirecting it to the
// DLL's replacement. The patch is done after --delayms (past VMProtect's startup integrity check); the function
// is not re-verified after boot (the Windows helper patches it too).
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

// Patch `target` in the process to jump to `patchfn` (14 bytes: jmp qword ptr [rip+0]; <patchfn>). -> ok.
static int patch(HANDLE proc, uintptr_t target, uintptr_t patchfn, const char *log_path) {
    unsigned char jmp[14] = {0xFF, 0x25, 0x00, 0x00, 0x00, 0x00};
    memcpy(jmp + 6, &patchfn, 8);
    void *addr = (void *)target;
    DWORD old;
    if (!VirtualProtectEx(proc, addr, sizeof(jmp), PAGE_EXECUTE_READWRITE, &old)) {
        printf("armer: VirtualProtectEx(RWX) failed %lu\n", GetLastError()); return 0;
    }
    SIZE_T wr = 0;
    BOOL ok = WriteProcessMemory(proc, addr, jmp, sizeof(jmp), &wr);
    DWORD old2;
    VirtualProtectEx(proc, addr, sizeof(jmp), old, &old2);
    FlushInstructionCache(proc, addr, sizeof(jmp));
    unsigned char back[14] = {0}; SIZE_T rd = 0;
    ReadProcessMemory(proc, addr, back, sizeof(back), &rd);
    int good = ok && wr == sizeof(jmp) && memcmp(back, jmp, sizeof(jmp)) == 0;
    (void)log_path;
    printf("armer: patch %s (%zu bytes)\n", good ? "ok" : "FAILED", wr);
    return good;
}

int main(int argc, char **argv) {
    DWORD pid = 0, delayms = 8000;
    const char *log = NULL;
    uintptr_t page = 0, target = 0, patchfn = 0;
    for (int i = 1; i + 1 < argc; i++) {
        if (!strcmp(argv[i], "--pid")) pid = (DWORD)strtoul(argv[++i], NULL, 10);
        else if (!strcmp(argv[i], "--log")) log = argv[++i];
        else if (!strcmp(argv[i], "--page")) page = (uintptr_t)strtoull(argv[++i], NULL, 0);
        else if (!strcmp(argv[i], "--target")) target = (uintptr_t)strtoull(argv[++i], NULL, 0);
        else if (!strcmp(argv[i], "--patchfn")) patchfn = (uintptr_t)strtoull(argv[++i], NULL, 0);
        else if (!strcmp(argv[i], "--delayms")) delayms = (DWORD)strtoul(argv[++i], NULL, 0);
    }
    if (!pid || !log) { printf("usage: armer.exe --pid <pid> --log <log> [--page <hex> | --target <hex> --patchfn <hex>]\n"); return 1; }
    DWORD access = patchfn ? (PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ | PROCESS_QUERY_INFORMATION)
                           : (PROCESS_VM_OPERATION | PROCESS_QUERY_INFORMATION);
    HANDLE proc = OpenProcess(access, FALSE, pid);
    if (!proc) { printf("armer: OpenProcess(%lu) failed %lu\n", pid, GetLastError()); return 1; }

    // the DLL must have installed its hook / unpacked the code before we touch it
    while (!log_has(log, "attach")) { if (!alive(proc)) return 2; Sleep(100); }

    if (patchfn) {                                // ---- patch mode ----
        if (!target) { printf("armer: --patchfn needs --target\n"); return 1; }
        for (DWORD w = 0; w < delayms; w += 100) { if (!alive(proc)) return 2; Sleep(100); }  // integrity window
        if (!alive(proc)) return 2;
        int ok = patch(proc, target, patchfn, log);
        fflush(stdout);
        return ok ? 0 : 1;
    }

    // ---- guard mode (Windows) ----
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
