#include "hooks.h"
#include "codepatch.h"
#include "crashlog.h"
#include "powder_api.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <cstring>
#include <mutex>

extern "C" {
#include "trampoline.h"
#include "buffer.h"
}

// The mid-hook stub, copied per hook: save flags, the 16 GPRs and xmm0-5 as a PowderRegs on the stack, call
// cb(regs, user) on an aligned stack, restore everything (rsp excepted), continue in the trampoline. The three
// slots at the end are filled per copy (rip-relative, so the copy works anywhere).
asm(R"(
.intel_syntax noprefix
.section .text
.globl pw_mid_tpl_start
pw_mid_tpl_start:
    pushfq
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rdi
    push rsi
    push rbp
    push rsp
    push rbx
    push rdx
    push rcx
    push rax
    lea rax, [rsp+136]
    mov [rsp+32], rax
    sub rsp, 96
    movdqu [rsp], xmm0
    movdqu [rsp+16], xmm1
    movdqu [rsp+32], xmm2
    movdqu [rsp+48], xmm3
    movdqu [rsp+64], xmm4
    movdqu [rsp+80], xmm5
    mov rcx, rsp
    mov rdx, qword ptr [rip+pw_mid_tpl_user]
    mov rbx, rsp
    and rsp, -16
    sub rsp, 32
    call qword ptr [rip+pw_mid_tpl_cb]
    mov rsp, rbx
    movdqu xmm0, [rsp]
    movdqu xmm1, [rsp+16]
    movdqu xmm2, [rsp+32]
    movdqu xmm3, [rsp+48]
    movdqu xmm4, [rsp+64]
    movdqu xmm5, [rsp+80]
    add rsp, 96
    pop rax
    pop rcx
    pop rdx
    pop rbx
    add rsp, 8
    pop rbp
    pop rsi
    pop rdi
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15
    popfq
    jmp qword ptr [rip+pw_mid_tpl_tramp]
.globl pw_mid_tpl_tramp
pw_mid_tpl_tramp: .quad 0
.globl pw_mid_tpl_cb
pw_mid_tpl_cb: .quad 0
.globl pw_mid_tpl_user
pw_mid_tpl_user: .quad 0
.globl pw_mid_tpl_end
pw_mid_tpl_end:
.att_syntax prefix
)");
extern "C" const uint8_t pw_mid_tpl_start[], pw_mid_tpl_tramp[], pw_mid_tpl_cb[], pw_mid_tpl_user[], pw_mid_tpl_end[];

static_assert(offsetof(PowderRegs, rax) == 96 && offsetof(PowderRegs, rflags) == 96 + 16 * 8,
              "PowderRegs must match the stub's stack layout");

namespace pw {

namespace {

struct Hook {
    int id;
    std::string owner, kind;
    uintptr_t target, patch_at;
    uint8_t backup[8];
    size_t patch_len;
    bool enabled;
};

std::mutex g_m;
std::vector<Hook> g_hooks;
int g_next = 1;

// Build the trampoline for target -> detour (not yet patched).
bool prepare(uintptr_t target, void *detour, TRAMPOLINE &ct, std::string &err) {
    void *buf = AllocateBuffer((LPVOID)target);
    if (!buf) { err = "no free memory near the target"; return false; }
    memset(&ct, 0, sizeof(ct));
    ct.pTarget = (LPVOID)target;
    ct.pDetour = detour;
    ct.pTrampoline = buf;
    crashlog_add_range((uintptr_t)buf, (uintptr_t)buf + 64);    // MinHook's slot: trampoline + relay
    if (!CreateTrampolineFunction(&ct)) {
        FreeBuffer(buf);
        err = "cannot relocate the target's first instructions (too short, or jumps into them)";
        return false;
    }
    return true;
}

// Write the jmp to the relay (MinHook's EnableHookLL, with our code writer) and record the hook.
int commit(const std::string &owner, const char *kind, uintptr_t target, const TRAMPOLINE &ct, std::string &err) {
    uint8_t patch[7];
    uintptr_t at = target;
    size_t n = 5;
    if (ct.patchAbove) { at = target - 5; n = 7; }     // too short a function: jmp from the padding above it
    patch[0] = 0xE9;
    int32_t rel = (int32_t)((intptr_t)ct.pRelay - (intptr_t)(at + 5));
    memcpy(patch + 1, &rel, 4);
    if (ct.patchAbove) { patch[5] = 0xEB; patch[6] = (uint8_t)(0 - 7); }
    Hook h{};
    h.owner = owner; h.kind = kind; h.target = target; h.patch_at = at; h.patch_len = n;
    memcpy(h.backup, (void *)at, n);
    std::string how;
    if (!write_code(at, patch, n, how)) {
        FreeBuffer(ct.pTrampoline);
        err = "could not patch the code (" + how + ")";
        return 0;
    }
    std::lock_guard<std::mutex> lk(g_m);
    h.id = g_next++;
    h.enabled = true;
    g_hooks.push_back(h);
    return h.id;
}

bool already_hooked(uintptr_t target) {
    std::lock_guard<std::mutex> lk(g_m);
    for (auto &h : g_hooks) if (h.enabled && h.target == target) return true;
    return false;
}

} // namespace

int hook_function(const std::string &owner, uintptr_t target, void *detour, void **original, std::string &err) {
    if (already_hooked(target)) { err = "already hooked"; return 0; }
    TRAMPOLINE ct;
    if (!prepare(target, detour, ct, err)) return 0;
    if (original) *original = ct.pTrampoline;          // set before the patch: the detour may run at once
    return commit(owner, "function", target, ct, err);
}

int hook_mid(const std::string &owner, uintptr_t at, void (*fn)(PowderRegs *, void *), void *user, std::string &err) {
    if (already_hooked(at)) { err = "already hooked"; return 0; }
    size_t size = pw_mid_tpl_end - pw_mid_tpl_start;
    uint8_t *stub = (uint8_t *)VirtualAlloc(nullptr, size, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
    if (!stub) { err = "no memory for the stub"; return 0; }
    memcpy(stub, pw_mid_tpl_start, size);
    crashlog_add_range((uintptr_t)stub, (uintptr_t)stub + size);
    TRAMPOLINE ct;
    if (!prepare(at, stub, ct, err)) { VirtualFree(stub, 0, MEM_RELEASE); return 0; }
    uint64_t slots[3] = {(uint64_t)ct.pTrampoline, (uint64_t)fn, (uint64_t)user};
    memcpy(stub + (pw_mid_tpl_tramp - pw_mid_tpl_start), &slots[0], 8);
    memcpy(stub + (pw_mid_tpl_cb - pw_mid_tpl_start), &slots[1], 8);
    memcpy(stub + (pw_mid_tpl_user - pw_mid_tpl_start), &slots[2], 8);
    FlushInstructionCache(GetCurrentProcess(), stub, size);
    return commit(owner, "mid", at, ct, err);
}

bool unhook(int id, std::string &err) {
    Hook h;
    {
        std::lock_guard<std::mutex> lk(g_m);
        auto it = g_hooks.begin();
        for (; it != g_hooks.end() && it->id != id; ++it) {}
        if (it == g_hooks.end() || !it->enabled) { err = "no such hook"; return false; }
        h = *it;
    }
    std::string how;
    if (!write_code(h.patch_at, h.backup, h.patch_len, how)) { err = "could not restore (" + how + ")"; return false; }
    std::lock_guard<std::mutex> lk(g_m);
    for (auto &x : g_hooks) if (x.id == id) x.enabled = false;
    return true;
}

std::vector<HookInfo> hook_list() {
    std::lock_guard<std::mutex> lk(g_m);
    std::vector<HookInfo> out;
    for (auto &h : g_hooks) out.push_back({h.id, h.owner, h.kind, h.target, h.enabled});
    return out;
}

} // namespace pw
