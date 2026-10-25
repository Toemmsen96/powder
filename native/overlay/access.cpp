#include "access.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <psapi.h>
#include <mutex>
#include <map>
#include <algorithm>
#include <cstdio>
#include <cstring>

extern "C" {
#include "hde64.h"
}

namespace pw {

AccessFinder g_access;

static std::mutex g_m;
static std::map<uint64_t, AccessHit> g_hits;     // keyed by RIP
static std::map<uint64_t, std::vector<uint8_t>> g_orig;   // rip -> original bytes (for NOP restore)
static PVOID g_veh = nullptr;
static uint64_t g_addr = 0, g_lo = 0, g_hi = 0, g_page = 0;
static SIZE_T g_pagesize = 0x1000;
static thread_local bool t_rearm = false;        // this thread set TF to step past a guarded access

static const char *REG_NAMES[16] = {"rax","rcx","rdx","rbx","rsp","rbp","rsi","rdi",
                                    "r8","r9","r10","r11","r12","r13","r14","r15"};

static uint64_t reg_val(const CONTEXT *c, int idx) {
    switch (idx) {
        case 0: return c->Rax; case 1: return c->Rcx; case 2: return c->Rdx; case 3: return c->Rbx;
        case 4: return c->Rsp; case 5: return c->Rbp; case 6: return c->Rsi; case 7: return c->Rdi;
        case 8: return c->R8;  case 9: return c->R9;  case 10: return c->R10; case 11: return c->R11;
        case 12: return c->R12;case 13: return c->R13;case 14: return c->R14; case 15: return c->R15;
    }
    return 0;
}

// Decode the memory operand of the instruction at rip: base register (or -1 for RIP-relative), its value,
// and the displacement. Fills a short text description.
static void decode(const CONTEXT *ctx, uint64_t rip, hde64s &hs, AccessHit &h) {
    uint8_t code[16];
    memcpy(code, (void *)rip, 16);
    hde64_disasm(code, &hs);
    h.len = hs.len ? hs.len : 1;

    long disp = 0;
    if (hs.flags & F_DISP8) disp = (int8_t)hs.disp.disp8;
    else if (hs.flags & F_DISP32) disp = (int32_t)hs.disp.disp32;
    h.disp = disp;

    int mod = hs.modrm_mod, rm = hs.modrm_rm;
    char buf[96];
    if (hs.flags & F_SIB) {
        int sb = hs.sib_base | (hs.rex_b << 3);
        if (hs.sib_base == 5 && mod == 0) { h.base_reg = -1; h.base_val = 0; }
        else { h.base_reg = sb; h.base_val = reg_val(ctx, sb); }
        snprintf(buf, sizeof(buf), "[%s+idx+0x%lx]", h.base_reg >= 0 ? REG_NAMES[h.base_reg] : "?", disp);
    } else if (mod == 0 && rm == 5) {
        h.base_reg = -1; h.base_val = rip + h.len;              // RIP-relative
        snprintf(buf, sizeof(buf), "[rip+0x%lx]", disp);
    } else {
        int br = rm | (hs.rex_b << 3);
        h.base_reg = br; h.base_val = reg_val(ctx, br);
        snprintf(buf, sizeof(buf), "[%s+0x%lx]", REG_NAMES[br], disp);
    }
    h.text = buf;
}

static void fill_module(AccessHit &h) {
    HMODULE mod = nullptr;
    if (GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           (LPCWSTR)h.rip, &mod) && mod) {
        wchar_t path[MAX_PATH];
        if (GetModuleFileNameW(mod, path, MAX_PATH)) {
            const wchar_t *base = wcsrchr(path, L'\\');
            base = base ? base + 1 : path;
            char nm[128]; WideCharToMultiByte(CP_UTF8, 0, base, -1, nm, sizeof(nm), nullptr, nullptr);
            h.module = nm;
        }
        h.module_off = h.rip - (uint64_t)mod;
    } else {
        h.module = "(dynamic)";
        h.module_off = h.rip;
    }
}

static LONG CALLBACK veh(EXCEPTION_POINTERS *ep) {
    DWORD code = ep->ExceptionRecord->ExceptionCode;
    if (code == STATUS_GUARD_PAGE_VIOLATION) {
        ULONG_PTR acc = ep->ExceptionRecord->ExceptionInformation[1];   // accessed address
        if (acc >= g_lo && acc < g_hi) {
            uint64_t rip = ep->ContextRecord->Rip;
            bool write = ep->ExceptionRecord->ExceptionInformation[0] == 1;
            std::lock_guard<std::mutex> lk(g_m);
            auto it = g_hits.find(rip);
            if (it == g_hits.end()) {
                AccessHit h; h.rip = rip; h.write = write; h.count = 1;
                hde64s hs;
                decode(ep->ContextRecord, rip, hs, h);   // rip points at valid code; reading 16 bytes is safe
                fill_module(h);
                g_hits[rip] = h;
            } else {
                it->second.count++;
                it->second.write = it->second.write || write;
            }
        }
        // the guard is one-shot and now cleared; step past this instruction, then re-arm on the single step
        ep->ContextRecord->EFlags |= 0x100;   // trap flag
        t_rearm = true;
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    if (code == EXCEPTION_SINGLE_STEP && t_rearm) {
        t_rearm = false;
        DWORD old;
        VirtualProtect((void *)g_page, g_pagesize, PAGE_READWRITE | PAGE_GUARD, &old);
        return EXCEPTION_CONTINUE_EXECUTION;
    }
    return EXCEPTION_CONTINUE_SEARCH;
}

bool AccessFinder::arm(uint64_t addr, size_t size) {
    disarm();
    SYSTEM_INFO si; GetSystemInfo(&si);
    g_pagesize = si.dwPageSize;
    g_addr = addr; g_lo = addr; g_hi = addr + size;
    g_page = addr & ~((uint64_t)g_pagesize - 1);
    { std::lock_guard<std::mutex> lk(g_m); g_hits.clear(); }
    g_veh = AddVectoredExceptionHandler(1, veh);
    if (!g_veh) { status_ = "could not install exception handler"; return false; }
    DWORD old;
    if (!VirtualProtect((void *)g_page, g_pagesize, PAGE_READWRITE | PAGE_GUARD, &old)) {
        RemoveVectoredExceptionHandler(g_veh); g_veh = nullptr;
        status_ = "could not guard the page (VMProtect blocked it?)"; return false;
    }
    armed_ = true; target_ = addr; status_ = "watching; move/act to capture writes";
    return true;
}

void AccessFinder::disarm() {
    if (g_veh) {
        DWORD old;
        VirtualProtect((void *)g_page, g_pagesize, PAGE_READWRITE, &old);   // drop the guard
        RemoveVectoredExceptionHandler(g_veh);
        g_veh = nullptr;
    }
    armed_ = false;
    status_ = "";
}

std::vector<AccessHit> AccessFinder::hits() const {
    std::lock_guard<std::mutex> lk(g_m);
    std::vector<AccessHit> out;
    out.reserve(g_hits.size());
    for (auto &kv : g_hits) out.push_back(kv.second);
    std::sort(out.begin(), out.end(), [](const AccessHit &a, const AccessHit &b) { return a.count > b.count; });
    return out;
}

void AccessFinder::clear() {
    std::lock_guard<std::mutex> lk(g_m);
    g_hits.clear();
}

void AccessFinder::nop(uint64_t rip) {
    std::lock_guard<std::mutex> lk(g_m);
    auto it = g_hits.find(rip);
    if (it == g_hits.end()) return;
    uint8_t len = it->second.len ? it->second.len : 1;
    DWORD old;
    if (!VirtualProtect((void *)rip, len, PAGE_EXECUTE_READWRITE, &old)) return;
    if (!g_orig.count(rip)) {
        std::vector<uint8_t> o(len);
        memcpy(o.data(), (void *)rip, len);
        g_orig[rip] = o;
    }
    memset((void *)rip, 0x90, len);     // NOP sled
    DWORD tmp; VirtualProtect((void *)rip, len, old, &tmp);
    FlushInstructionCache(GetCurrentProcess(), (void *)rip, len);
    it->second.nopped = true;
}

void AccessFinder::restore_all() {
    std::lock_guard<std::mutex> lk(g_m);
    for (auto &kv : g_orig) {
        DWORD old;
        if (VirtualProtect((void *)kv.first, kv.second.size(), PAGE_EXECUTE_READWRITE, &old)) {
            memcpy((void *)kv.first, kv.second.data(), kv.second.size());
            DWORD tmp; VirtualProtect((void *)kv.first, kv.second.size(), old, &tmp);
            FlushInstructionCache(GetCurrentProcess(), (void *)kv.first, kv.second.size());
        }
        auto it = g_hits.find(kv.first);
        if (it != g_hits.end()) it->second.nopped = false;
    }
    g_orig.clear();
}

} // namespace pw
