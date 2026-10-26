#include "crashlog.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cstdio>
#include <cstring>

namespace pw {

namespace {

wchar_t g_path[MAX_PATH];
LPTOP_LEVEL_EXCEPTION_FILTER g_prev = nullptr;
std::atomic<int> g_logged{0};               // vectored lines, capped
std::atomic<bool> g_in_filter{false};

struct Range { uintptr_t lo, hi; };
Range g_ranges[256];
std::atomic<int> g_nranges{0};
uintptr_t g_self_lo = 0, g_self_hi = 0;

void write_line(const char *s) {
    HANDLE f = CreateFileW(g_path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_ALWAYS, 0, nullptr);
    if (f == INVALID_HANDLE_VALUE) return;
    SYSTEMTIME t; GetLocalTime(&t);
    char line[600];
    int n = snprintf(line, sizeof(line), "%02d:%02d:%02d.%03d overlay: %s\r\n", t.wHour, t.wMinute, t.wSecond,
                     t.wMilliseconds, s);
    DWORD wr;
    WriteFile(f, line, n > 0 && n < (int)sizeof(line) ? n : (int)strlen(line), &wr, nullptr);
    CloseHandle(f);
}

// "module+0xoff" for an address in a loaded image, else the address.
void where(uintptr_t a, char *out, size_t n) {
    HMODULE m = nullptr;
    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           (LPCWSTR)a, &m) && m) {
        char path[MAX_PATH];
        GetModuleFileNameA(m, path, MAX_PATH);
        const char *base = strrchr(path, '\\');
        snprintf(out, n, "%s+0x%llx", base ? base + 1 : path, (unsigned long long)(a - (uintptr_t)m));
    } else {
        snprintf(out, n, "%p", (void *)a);
    }
}

bool ours(uintptr_t a) {
    if (a >= g_self_lo && a < g_self_hi) return true;
    int n = g_nranges.load();
    for (int i = 0; i < n; i++) if (a >= g_ranges[i].lo && a < g_ranges[i].hi) return true;
    HMODULE m = nullptr;                    // client mods: DLLs loaded from powder_mods\.
    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           (LPCWSTR)a, &m) && m) {
        wchar_t path[MAX_PATH];
        GetModuleFileNameW(m, path, MAX_PATH);
        return wcsstr(path, L"\\powder_mods\\") != nullptr;
    }
    return false;
}

bool executable(uintptr_t a) {
    MEMORY_BASIC_INFORMATION m;
    if (!VirtualQuery((void *)a, &m, sizeof(m)) || m.State != MEM_COMMIT) return false;
    DWORD p = m.Protect & 0xFF;
    return p == PAGE_EXECUTE || p == PAGE_EXECUTE_READ || p == PAGE_EXECUTE_READWRITE || p == PAGE_EXECUTE_WRITECOPY;
}

void describe(const char *what, EXCEPTION_POINTERS *ep) {
    const EXCEPTION_RECORD *er = ep->ExceptionRecord;
    const CONTEXT *c = ep->ContextRecord;
    char at[200], line[600];
    where((uintptr_t)er->ExceptionAddress, at, sizeof(at));
    if (er->ExceptionCode == EXCEPTION_ACCESS_VIOLATION && er->NumberParameters >= 2)
        snprintf(line, sizeof(line), "%s: 0x%08lx at %s (%s %p) thread %lu", what, er->ExceptionCode, at,
                 er->ExceptionInformation[0] == 1 ? "writing" : er->ExceptionInformation[0] == 8 ? "executing"
                                                                                                 : "reading",
                 (void *)er->ExceptionInformation[1], GetCurrentThreadId());
    else
        snprintf(line, sizeof(line), "%s: 0x%08lx at %s thread %lu", what, er->ExceptionCode, at, GetCurrentThreadId());
    write_line(line);
    snprintf(line, sizeof(line), "  rax=%llx rbx=%llx rcx=%llx rdx=%llx rsi=%llx rdi=%llx rbp=%llx rsp=%llx",
             c->Rax, c->Rbx, c->Rcx, c->Rdx, c->Rsi, c->Rdi, c->Rbp, c->Rsp);
    write_line(line);
    snprintf(line, sizeof(line), "  r8=%llx r9=%llx r10=%llx r11=%llx r12=%llx r13=%llx r14=%llx r15=%llx",
             c->R8, c->R9, c->R10, c->R11, c->R12, c->R13, c->R14, c->R15);
    write_line(line);
    // return addresses on the stack: values that point into executable memory
    int found = 0;
    size_t len = 0;
    line[0] = 0;
    for (int i = 0; i < 512 && found < 14; i++) {
        uintptr_t v = 0;
        SIZE_T got = 0;
        if (!ReadProcessMemory(GetCurrentProcess(), (void *)(c->Rsp + 8 * i), &v, 8, &got) || got != 8) break;
        if (v < 0x10000 || !executable(v)) continue;
        char w[160];
        where(v, w, sizeof(w));
        len += snprintf(line + len, sizeof(line) - len, " %s", w);
        if (len > sizeof(line) - 160) break;
        found++;
    }
    char head[640];
    snprintf(head, sizeof(head), "  stack:%s", line);
    write_line(head);
}

LONG CALLBACK veh(EXCEPTION_POINTERS *ep) {
    DWORD code = ep->ExceptionRecord->ExceptionCode;
    bool fault = code == EXCEPTION_ACCESS_VIOLATION || code == EXCEPTION_ILLEGAL_INSTRUCTION ||
                 code == EXCEPTION_PRIV_INSTRUCTION || code == EXCEPTION_STACK_OVERFLOW ||
                 code == EXCEPTION_INT_DIVIDE_BY_ZERO;
    if (fault && ours((uintptr_t)ep->ExceptionRecord->ExceptionAddress) && g_logged.fetch_add(1) < 20)
        describe("FAULT IN POWDER CODE", ep);
    return EXCEPTION_CONTINUE_SEARCH;
}

LONG WINAPI filter(EXCEPTION_POINTERS *ep) {
    if (!g_in_filter.exchange(true)) describe("GAME CRASH (unhandled exception)", ep);
    return g_prev ? g_prev(ep) : EXCEPTION_CONTINUE_SEARCH;
}

} // namespace

void crashlog_add_range(uintptr_t lo, uintptr_t hi) {
    int i = g_nranges.load();
    if (i >= 256) return;
    g_ranges[i] = {lo, hi};
    g_nranges.store(i + 1);
}

void crashlog_install(const wchar_t *logpath) {
    wcsncpy(g_path, logpath, MAX_PATH - 1);
    HMODULE self = nullptr;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                       (LPCWSTR)&crashlog_install, &self);
    if (self) {
        auto *dos = (IMAGE_DOS_HEADER *)self;
        auto *nt = (IMAGE_NT_HEADERS64 *)((uint8_t *)self + dos->e_lfanew);
        g_self_lo = (uintptr_t)self;
        g_self_hi = g_self_lo + nt->OptionalHeader.SizeOfImage;
    }
    AddVectoredExceptionHandler(1, veh);
    g_prev = SetUnhandledExceptionFilter(filter);
}

} // namespace pw
