// tls_hook.h - the shared core of Powder's TLS helper: trap Steep's certificate check (OpenSSL 1.0.2n's
// ssl_verify_cert_chain) with a guarded page + vectored exception handler, and accept the chain only when the
// server certificate is the one the launcher pinned. Included by hook_guard.c (Windows, armed by armer.exe
// from outside) and version_proxy.c (Linux/Proton, armed in-process from a proxy DLL). The comments in those
// files explain how each is loaded and armed; this header is the logic they share.
//
// Offsets are for the supported Steep.exe build, read from its own OpenSSL 1.0.2n ASN.1 item tables
// (STACK_OF(X509) data @0x08, X509 cert_info @0x00, X509_CINF cached tbsCertificate @0x50). The function is
// at image base + TLS_TARGET_RVA; its first 16 bytes (TLS_PROLOGUE) identify it and tell when VMProtect has
// finished unpacking the code.

#ifndef POWDER_TLS_HOOK_H
#define POWDER_TLS_HOOK_H

#include <windows.h>
#include <bcrypt.h>
#include <stdint.h>
#include <stdio.h>
#include <stdarg.h>
#include <string.h>
#include <wchar.h>

#define TLS_TARGET_RVA 0x1D3E290ULL
#define TLS_GUARD_VIOLATION 0x80000001L
#define TLS_PROLOGUE_LEN 16
#define TLS_MAX_TBS (1 << 20)          // a tbsCertificate is a few KB; cap the read well above that

// OpenSSL 1.0.2n on Win64 (long is 32 bits):
#define TLS_OFF_STACK_DATA 0x08        // STACK_OF(X509): void **data
#define TLS_OFF_X509_CINF  0x00        // X509: X509_CINF *cert_info
#define TLS_OFF_CINF_ENC   0x50        // X509_CINF: cached tbsCertificate bytes (uint8_t *)
#define TLS_OFF_CINF_LEN   0x58        // X509_CINF: its length (long)

// ssl_verify_cert_chain's first instructions (no rip-relative operands): mov [rsp+8],rbx; mov [rsp+0x10],rsi;
// push rdi; mov eax,0x130. Present once VMProtect has unpacked the code.
static const unsigned char TLS_PROLOGUE[TLS_PROLOGUE_LEN] = {
    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x57, 0xB8, 0x30, 0x01, 0x00, 0x00};

static uintptr_t g_target = 0, g_page = 0;
static SIZE_T g_pagesize = 0x1000;
static volatile LONG g_hits = 0;      // times ssl_verify_cert_chain itself was trapped
static volatile LONG g_faults = 0;    // all guard faults on the page (any code on it): is the handler firing?
static wchar_t g_log[MAX_PATH];
static unsigned char g_pin[32];
static int g_have_pin = 0;

static void tls_log(const char *fmt, ...) {
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
static int tls_readable(const void *p, SIZE_T n) {
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

static int tls_sha256(const unsigned char *data, ULONG len, unsigned char out[32]) {
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
static int tls_verify(const void *sk) {
    if (!g_have_pin || !tls_readable(sk, 0x10)) return 0;
    int num = *(const int *)sk;
    const void *const *data = *(const void *const *const *)((const char *)sk + TLS_OFF_STACK_DATA);
    if (num < 1 || !tls_readable(data, sizeof(void *))) return 0;
    const char *leaf = (const char *)data[0];
    if (!tls_readable(leaf, TLS_OFF_X509_CINF + sizeof(void *))) return 0;
    const char *cinf = *(const char *const *)(leaf + TLS_OFF_X509_CINF);
    if (!tls_readable(cinf, TLS_OFF_CINF_LEN + sizeof(long))) return 0;
    const unsigned char *tbs = *(const unsigned char *const *)(cinf + TLS_OFF_CINF_ENC);
    long len = *(const long *)(cinf + TLS_OFF_CINF_LEN);
    if (len <= 0 || len > TLS_MAX_TBS || !tls_readable(tbs, (SIZE_T)len)) return 0;
    unsigned char h[32];
    if (!tls_sha256(tbs, (ULONG)len, h)) return 0;
    return memcmp(h, g_pin, 32) == 0;
}

static LONG CALLBACK tls_veh(PEXCEPTION_POINTERS ep) {
    if (ep->ExceptionRecord->ExceptionCode != TLS_GUARD_VIOLATION) return EXCEPTION_CONTINUE_SEARCH;
    InterlockedIncrement(&g_faults);
    CONTEXT *c = ep->ContextRecord;
    if (c->Rip == g_target) {
        // entering ssl_verify_cert_chain(s=rcx, sk=rdx): decide here and return to the caller without running it
        int ok = tls_verify((const void *)c->Rdx);
        uintptr_t ret = *(uintptr_t *)c->Rsp;
        c->Rsp += 8; c->Rip = ret; c->Rax = (DWORD64)(ok ? 1 : 0);
        LONG n = InterlockedIncrement(&g_hits);
        if (n <= 5 || (n % 50) == 0) tls_log("hook hit #%ld -> %s", n, ok ? "server cert pinned, accept" : "rejected");
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    // any other access to the page (incl. VMProtect's integrity read): the guard is already cleared, so the
    // access proceeds, and the caller re-arms it
    return EXCEPTION_CONTINUE_EXECUTION;
}

// Compute g_target / g_page from the main module's base.
static void tls_init_target(void) {
    SYSTEM_INFO si; GetSystemInfo(&si); g_pagesize = si.dwPageSize;
    uintptr_t base = (uintptr_t)GetModuleHandleW(NULL);
    g_target = base + TLS_TARGET_RVA;
    g_page = g_target & ~((uintptr_t)g_pagesize - 1);
}

// Re-arm the guard on the function's page; -> whether VirtualProtect succeeded. (In-process arming is blocked
// by VMProtect after an initial window, so the guard is kept armed by armer.exe from outside; this is only the
// first arm on Windows.)
static int tls_arm(void) __attribute__((unused));
static int tls_arm(void) {
    DWORD old;
    return VirtualProtect((void *)g_page, g_pagesize, PAGE_EXECUTE_READ | PAGE_GUARD, &old) != 0;
}

// Whether ssl_verify_cert_chain is unpacked (its prologue is in place). False while VMProtect is still
// unpacking that page, or if the build does not match. (Used by the Linux proxy, not the Windows build.)
static int tls_unpacked(void) __attribute__((unused));
static int tls_unpacked(void) {
    return tls_readable((void *)g_target, TLS_PROLOGUE_LEN) &&
           memcmp((void *)g_target, TLS_PROLOGUE, TLS_PROLOGUE_LEN) == 0;
}

// Build "<directory of module_path>\name" into out (MAX_PATH). module_path is a GetModuleFileNameW result.
static void tls_sibling(const wchar_t *module_path, const wchar_t *name, wchar_t *out) __attribute__((unused));
static void tls_sibling(const wchar_t *module_path, const wchar_t *name, wchar_t *out) {
    wcscpy(out, module_path);
    wchar_t *slash = wcsrchr(out, L'\\');
    wcscpy(slash ? slash + 1 : out, name);
}

// Load the pin from `path`: exactly 32 bytes, the pinned leaf's tbsCertificate SHA-256. Without it
// tls_verify() rejects every certificate.
static void tls_load_pin_at(const wchar_t *path) {
    HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) { tls_log("no pin file: every certificate is rejected"); return; }
    unsigned char buf[33]; DWORD got = 0;
    BOOL ok = ReadFile(f, buf, sizeof(buf), &got, NULL);
    CloseHandle(f);
    if (ok && got == 32) { memcpy(g_pin, buf, 32); g_have_pin = 1; }
    else tls_log("pin file is not 32 bytes (%lu): every certificate is rejected", got);
}

// Load the pin from the module's path with .pin instead of its extension (`ext_at` = index of the dot).
static void tls_load_pin(const wchar_t *dll_path, DWORD ext_at) __attribute__((unused));
static void tls_load_pin(const wchar_t *dll_path, DWORD ext_at) {
    wchar_t path[MAX_PATH];
    if (ext_at < 1 || ext_at + 4 >= MAX_PATH) return;
    wcsncpy(path, dll_path, ext_at);
    wcscpy(path + ext_at, L".pin");
    tls_load_pin_at(path);
}

#endif // POWDER_TLS_HOOK_H
