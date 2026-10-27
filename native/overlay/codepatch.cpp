#include "codepatch.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <cstring>

namespace pw {

static std::wstring g_armer;    // armer.exe beside the overlay (the TLS helper), for patching from outside

void code_set_helper(const wchar_t *armer_exe) { g_armer = armer_exe; }

// Free memory within +-2 GB of `target` (a rel32 jmp reaches it), committed RWX.
uint8_t *alloc_near(uintptr_t target, size_t size) {
    SYSTEM_INFO si; GetSystemInfo(&si);
    const uintptr_t gran = si.dwAllocationGranularity, span = 0x7FF00000;
    uintptr_t lo = target > span ? target - span : gran, hi = target + span;
    for (uintptr_t a = (target & ~(gran - 1)) - gran; a > lo; ) {            // downward first
        MEMORY_BASIC_INFORMATION m;
        if (!VirtualQuery((void *)a, &m, sizeof(m))) break;
        if (m.State == MEM_FREE) {
            if (void *p = VirtualAlloc((void *)a, size, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE))
                return (uint8_t *)p;
            a -= gran;
        } else {
            a = ((uintptr_t)m.AllocationBase & ~(gran - 1)) - gran;
        }
    }
    for (uintptr_t a = (target + gran) & ~(gran - 1); a < hi; ) {
        MEMORY_BASIC_INFORMATION m;
        if (!VirtualQuery((void *)a, &m, sizeof(m))) break;
        if (m.State == MEM_FREE) {
            uintptr_t at = ((uintptr_t)m.BaseAddress + gran - 1) & ~(gran - 1);
            if (void *p = VirtualAlloc((void *)at, size, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE))
                return (uint8_t *)p;
        }
        a = (uintptr_t)m.BaseAddress + m.RegionSize;
    }
    return nullptr;
}

static bool written(uintptr_t at, const uint8_t *src, size_t n) {
    FlushInstructionCache(GetCurrentProcess(), (void *)at, n);
    return memcmp((void *)at, src, n) == 0;
}

// Write code bytes. VMProtect blocks the game process's own writes to its code (VirtualProtect and
// WriteProcessMemory on itself fail in Steep; fine elsewhere), so after those armer.exe --write does it from
// outside (VirtualProtectEx + WriteProcessMemory, like its TLS patch). Wine can't write /proc/self/mem.
bool write_code(uintptr_t at, const uint8_t *src, size_t n, std::string &how) {
    SIZE_T done = 0;
    if (WriteProcessMemory(GetCurrentProcess(), (void *)at, src, n, &done) && done == n && written(at, src, n)) return true;
    how = "WriteProcessMemory " + std::to_string(GetLastError());
    DWORD old;
    if (VirtualProtect((void *)at, n, PAGE_EXECUTE_READWRITE, &old)) {
        memcpy((void *)at, src, n);
        DWORD tmp; VirtualProtect((void *)at, n, old, &tmp);
        if (written(at, src, n)) return true;
    }
    how += ", VirtualProtect " + std::to_string(GetLastError());
    if (!g_armer.empty()) {
        wchar_t cmd[512];
        std::wstring hex;
        for (size_t i = 0; i < n; i++) { wchar_t b[3]; swprintf(b, 3, L"%02x", src[i]); hex += b; }
        swprintf(cmd, 512, L"\"%ls\" --pid %lu --write %llx %ls", g_armer.c_str(), GetCurrentProcessId(),
                 (unsigned long long)at, hex.c_str());
        STARTUPINFOW si{}; si.cb = sizeof(si);
        PROCESS_INFORMATION pi{};
        if (CreateProcessW(nullptr, cmd, nullptr, nullptr, FALSE, CREATE_NO_WINDOW, nullptr, nullptr, &si, &pi)) {
            WaitForSingleObject(pi.hProcess, 10000);
            DWORD code = 1;
            GetExitCodeProcess(pi.hProcess, &code);
            CloseHandle(pi.hProcess); CloseHandle(pi.hThread);
            if (code == 0 && written(at, src, n)) return true;
            how += ", armer.exe exit " + std::to_string(code);
        } else {
            how += ", armer.exe not started " + std::to_string(GetLastError());
        }
    } else {
        how += ", no armer.exe";
    }
    return false;
}

} // namespace pw
