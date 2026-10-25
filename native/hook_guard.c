// hook_guard.dll - lets Steep accept the Powder server's own TLS certificate, and only that (play build).
//
// Steep verifies certificates with a statically linked OpenSSL 1.0.2n (ssl_verify_cert_chain, in Ubisoft's
// UbiServices SDK), and the exe is protected by VMProtect, which detects code patches and debug registers.
// So this DLL changes no code bytes: it traps execution of ssl_verify_cert_chain with a guarded memory page
// and a vectored exception handler. PAGE_GUARD is one-shot and VMProtect blocks in-process VirtualProtect on
// its ranges, so armer.exe re-arms the page from outside for the whole session.
//
// On each trapped call the handler runs verify() instead of the function and returns its result to the
// caller. verify() accepts the chain (returns 1) only when the server certificate (the chain's leaf) is
// byte-for-byte the one the launcher pinned for this server; otherwise it returns 0 and the game treats the
// connection as untrusted, exactly as it would without this DLL. The launcher writes the pin next to this
// DLL (hook_guard.pin: 32 bytes, SHA-256 of the pinned leaf's tbsCertificate) before injecting; it is the
// SHA-256 of the same bytes OpenSSL caches for the certificate it received, so a different certificate, or
// any certificate for any other host, never matches.
//
// This is the only behaviour change from the earlier helper, which returned 1 for every certificate (it
// disabled Steep's certificate check entirely). verify() fails closed: no pin file, a certificate it cannot
// read, or an unexpected layout all yield 0 (the game stays offline), never 1 and never a crash - every read
// of the game's OpenSSL structures is guarded with VirtualQuery first.
//
// Injected by the launcher (LoadLibraryW via CreateRemoteThread). Logs to hook_guard.log next to this DLL;
// armer.exe and the launcher read "attach" and "hook hit" from it.
//
// Only valid for the Steep.exe build whose ssl_verify_cert_chain is at image base + TARGET_RVA; the launcher
// checks the exe's SHA-256 and the function's first bytes before injecting. The OpenSSL 1.0.2n structure
// offsets below (STACK_OF(X509), X509, X509_CINF's cached tbsCertificate) were read from that build's own
// ASN.1 item tables.
//
//   zig cc -target x86_64-windows-gnu -shared -O2 -o bin/hook_guard.dll hook_guard.c -lbcrypt
//   (or x86_64-w64-mingw32-gcc -shared -O2 -o bin/hook_guard.dll hook_guard.c -lbcrypt)

#include <windows.h>
#include <bcrypt.h>
#include <stdint.h>
#include <stdio.h>
#include <stdarg.h>
#include <string.h>
#include <wchar.h>

#define TARGET_RVA 0x1D3E290ULL
#define GUARD_VIOLATION 0x80000001L

// OpenSSL 1.0.2n on Win64 (read from the build's ASN.1 item tables):
//   STACK_OF(X509): int num @0x00, void **data @0x08
//   X509:           X509_CINF *cert_info @0x00
//   X509_CINF:      ASN1_ENCODING enc @0x50, i.e. the cached tbsCertificate: uint8_t *enc @0x50, long len @0x58
#define OFF_STACK_DATA 0x08
#define OFF_X509_CINF  0x00
#define OFF_CINF_ENC   0x50
#define OFF_CINF_LEN   0x58
#define MAX_TBS (1 << 20)          // a certificate's tbsCertificate is a few KB; cap the read well above that

static uintptr_t g_target = 0, g_page = 0;
static SIZE_T g_pagesize = 0x1000;
static volatile LONG g_hits = 0;
static wchar_t g_log[MAX_PATH];
static unsigned char g_pin[32];
static int g_have_pin = 0;

static void logline(const char *fmt, ...) {
    char buf[512];
    va_list ap; va_start(ap, fmt); vsnprintf(buf, sizeof(buf), fmt, ap); va_end(ap);
    HANDLE f = CreateFileW(g_log, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) return;
    SYSTEMTIME t; GetLocalTime(&t);        // same clock as the server log, for lining them up
    char line[640];
    int n = snprintf(line, sizeof(line), "%02d:%02d:%02d.%03d %s\r\n", t.wHour, t.wMinute, t.wSecond,
                     t.wMilliseconds, buf);
    if (n < 0) return;
    if (n >= (int)sizeof(line)) n = (int)sizeof(line) - 1;
    DWORD w; SetFilePointer(f, 0, NULL, FILE_END); WriteFile(f, line, (DWORD)n, &w, NULL); CloseHandle(f);
}

// Whether every byte of [p, p+n) is committed and readable. The pointers come from the game's OpenSSL
// structures; a wrong guess about their layout must leave the game running, so nothing is dereferenced
// before this says so.
static int readable(const void *p, SIZE_T n) {
    if (!p || n == 0) return 0;
    const char *a = (const char *)p, *end = a + n;
    if (end < a) return 0;
    MEMORY_BASIC_INFORMATION mbi;
    while (a < end) {
        if (!VirtualQuery(a, &mbi, sizeof(mbi)) || mbi.State != MEM_COMMIT) return 0;
        DWORD ok = PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY |
                   PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
        if ((mbi.Protect & (PAGE_NOACCESS | PAGE_GUARD)) || !(mbi.Protect & ok)) return 0;
        a = (const char *)mbi.BaseAddress + mbi.RegionSize;
    }
    return 1;
}

static int sha256(const unsigned char *data, ULONG len, unsigned char out[32]) {
    BCRYPT_ALG_HANDLE alg = NULL;
    BCRYPT_HASH_HANDLE h = NULL;
    int ok = 0;
    if (BCryptOpenAlgorithmProvider(&alg, BCRYPT_SHA256_ALGORITHM, NULL, 0) < 0) return 0;
    if (BCryptCreateHash(alg, &h, NULL, 0, NULL, 0, 0) >= 0) {
        ok = BCryptHashData(h, (PUCHAR)data, len, 0) >= 0 && BCryptFinishHash(h, out, 32, 0) >= 0;
        BCryptDestroyHash(h);
    }
    BCryptCloseAlgorithmProvider(alg, 0);
    return ok;
}

// The server certificate (the chain's leaf) is the one the launcher pinned: its cached tbsCertificate hashes
// to g_pin. Returns 1 only then; 0 on any doubt (no pin, unreadable, mismatch). `sk` is ssl_verify_cert_chain's
// second argument, a STACK_OF(X509)* with the leaf at index 0.
static int verify(const void *sk) {
    if (!g_have_pin || !readable(sk, 0x10)) return 0;
    int num = *(const int *)sk;
    const void *const *data = *(const void *const *const *)((const char *)sk + OFF_STACK_DATA);
    if (num < 1 || !readable(data, sizeof(void *))) return 0;
    const char *leaf = (const char *)data[0];
    if (!readable(leaf, OFF_X509_CINF + sizeof(void *))) return 0;
    const char *cinf = *(const char *const *)(leaf + OFF_X509_CINF);
    if (!readable(cinf, OFF_CINF_LEN + sizeof(long))) return 0;
    const unsigned char *tbs = *(const unsigned char *const *)(cinf + OFF_CINF_ENC);
    long len = *(const long *)(cinf + OFF_CINF_LEN);
    if (len <= 0 || len > MAX_TBS || !readable(tbs, (SIZE_T)len)) return 0;
    unsigned char h[32];
    if (!sha256(tbs, (ULONG)len, h)) return 0;
    return memcmp(h, g_pin, 32) == 0;
}

static LONG CALLBACK veh(PEXCEPTION_POINTERS ep) {
    if (ep->ExceptionRecord->ExceptionCode != GUARD_VIOLATION) return EXCEPTION_CONTINUE_SEARCH;
    CONTEXT *c = ep->ContextRecord;
    if (c->Rip == g_target) {
        // entering ssl_verify_cert_chain(s=rcx, sk=rdx): decide here and return to the caller without running it
        int ok = verify((const void *)c->Rdx);
        uintptr_t ret = *(uintptr_t *)c->Rsp;
        c->Rsp += 8; c->Rip = ret; c->Rax = (DWORD64)(ok ? 1 : 0);
        LONG n = InterlockedIncrement(&g_hits);
        if (n <= 5 || (n % 50) == 0) logline("hook hit #%ld -> %s", n, ok ? "server cert pinned, accept" : "rejected");
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    // any other access to the page (incl. VMProtect's integrity read): the guard is already cleared, so the
    // access proceeds; armer.exe re-arms it right away
    return EXCEPTION_CONTINUE_EXECUTION;
}

// Load hook_guard.pin (this DLL's path with .pin instead of .dll): exactly 32 bytes, the pinned leaf's
// tbsCertificate SHA-256. Without it verify() rejects every certificate.
static void load_pin(const wchar_t *dll_path, DWORD name_len) {
    wchar_t path[MAX_PATH];
    if (name_len < 4 || name_len >= MAX_PATH) return;
    wcscpy(path, dll_path);
    wcscpy(path + name_len - 4, L".pin");
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) { logline("no pin file: every certificate is rejected"); return; }
    unsigned char buf[33]; DWORD got = 0;
    BOOL ok = ReadFile(f, buf, sizeof(buf), &got, NULL);
    CloseHandle(f);
    if (ok && got == 32) { memcpy(g_pin, buf, 32); g_have_pin = 1; }
    else logline("pin file is not 32 bytes (%lu): every certificate is rejected", got);
}

BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, LPVOID r) {
    (void)r;
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    DisableThreadLibraryCalls(h);
    DWORD n = GetModuleFileNameW(h, g_log, MAX_PATH);          // log file = this DLL's path, .dll -> .log
    if (n < 4 || n >= MAX_PATH) return TRUE;
    load_pin(g_log, n);
    wcscpy(g_log + n - 4, L".log");
    SYSTEM_INFO si; GetSystemInfo(&si); g_pagesize = si.dwPageSize;
    uintptr_t base = (uintptr_t)GetModuleHandleW(NULL);
    g_target = base + TARGET_RVA;
    g_page = g_target & ~((uintptr_t)g_pagesize - 1);
    AddVectoredExceptionHandler(1, veh);
    DWORD old;
    VirtualProtect((void *)g_page, g_pagesize, PAGE_EXECUTE_READ | PAGE_GUARD, &old);
    logline("attach: base=%p target=%p pid=%lu pin=%s", (void *)base, (void *)g_target, GetCurrentProcessId(),
            g_have_pin ? "yes" : "MISSING");
    return TRUE;
}
