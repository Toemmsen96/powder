#include "fixes.h"
#include "hooks.h"
#include "powder_api.h"
#include "json.hpp"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <vector>

namespace pw {

namespace {

alignas(64) uint8_t g_zero[4096];          // what a guarded NULL points at: every field reads 0
void (*g_log)(const char *) = nullptr;

struct Guard {
    std::string name;
    int reg;                                // index into PowderRegs' GPRs (rax = 0 ... r15 = 15)
    std::atomic<int> hits{0};
};
thread_local uint64_t t_args[2];            // rcx, rdx at the fix's args_at point (the failing call's inputs)

void on_args(PowderRegs *r, void *) { t_args[0] = r->rcx; t_args[1] = r->rdx; }
std::vector<Guard *> g_guards;

const char *REG_NAMES[16] = {"rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi",
                             "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"};

void on_guard(PowderRegs *r, void *user) {
    auto *g = (Guard *)user;
    uint64_t *gpr = &r->rax;
    if (gpr[g->reg] != 0) return;
    gpr[g->reg] = (uint64_t)g_zero;
    int n = g->hits.fetch_add(1);
    if (n < 5 && g_log) {
        char line[400];
        snprintf(line, sizeof(line), "fix '%s': %s was NULL (#%d) - guarded. call args rcx=%llx rdx=%llx; rbx=%llx "
                 "rsi=%llx rdi=%llx r12=%llx r13=%llx r14=%llx r15=%llx", g->name.c_str(), REG_NAMES[g->reg], n + 1,
                 (unsigned long long)t_args[0], (unsigned long long)t_args[1],
                 (unsigned long long)r->rbx, (unsigned long long)r->rsi, (unsigned long long)r->rdi,
                 (unsigned long long)r->r12, (unsigned long long)r->r13, (unsigned long long)r->r14,
                 (unsigned long long)r->r15);
        g_log(line);
    }
}

} // namespace

void fixes_apply(const wchar_t *trainer_json, void (*log)(const char *)) {
    g_log = log;
    FILE *f = _wfopen(trainer_json, L"rb");
    if (!f) return;
    std::string text;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof(buf), f)) > 0) text.append(buf, n);
    fclose(f);
    nlohmann::json fixes;
    try {
        fixes = nlohmann::json::parse(text).value("fixes", nlohmann::json::array());
    } catch (const std::exception &e) {
        log((std::string("fixes: ") + e.what()).c_str());
        return;
    }
    uintptr_t base = (uintptr_t)GetModuleHandleW(nullptr);
    for (auto &fx : fixes) {
        std::string name = fx.value("name", "?"), reg = fx.value("null_guard", "");
        uintptr_t at = base + std::stoull(fx.value("rva", "0"), nullptr, 16);
        std::string hex = fx.value("bytes", "");
        std::vector<uint8_t> want;
        for (size_t i = 0; i + 1 < hex.size(); i += 2) want.push_back((uint8_t)std::stoul(hex.substr(i, 2), nullptr, 16));
        int idx = -1;
        for (int i = 0; i < 16; i++) if (reg == REG_NAMES[i]) idx = i;
        char line[300];
        if (idx < 0 || idx == 4 || want.empty() || memcmp((void *)at, want.data(), want.size()) != 0) {
            snprintf(line, sizeof(line), "fix '%s' skipped (%s)", name.c_str(),
                     idx < 0 || idx == 4 ? "bad register" : "bytes differ: another game build?");
            log(line);
            continue;
        }
        auto *g = new Guard;
        g->name = name;
        g->reg = idx;
        std::string err;
        int id = hook_mid("fixes", at, on_guard, g, err);
        snprintf(line, sizeof(line), id ? "fix '%s' applied" : "fix '%s' failed: %s", name.c_str(), err.c_str());
        log(line);
        if (id) g_guards.push_back(g);
        if (id && fx.contains("args_at")) {        // optional: record the failing call's arguments for the log
            uintptr_t a = base + std::stoull(fx.value("args_at", "0"), nullptr, 16);
            std::string ab = fx.value("args_bytes", "");
            std::vector<uint8_t> w;
            for (size_t i = 0; i + 1 < ab.size(); i += 2) w.push_back((uint8_t)std::stoul(ab.substr(i, 2), nullptr, 16));
            if (!w.empty() && memcmp((void *)a, w.data(), w.size()) == 0 && hook_mid("fixes", a, on_args, nullptr, err))
                log("  (its call arguments are logged too)");
        }
    }
}

} // namespace pw
