#include "modhost.h"
#include "hooks.h"
#include "codepatch.h"
#include "noclip.h"
#include "powder_api.h"
#include "imgui.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <cstdarg>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <vector>

namespace pw {

namespace {

struct Panel { const PowderMod *mod; std::string title; PowderPanelFn draw; void *user; };
struct LoadedMod { PowderMod mod; std::string name, status; HMODULE dll; };

std::vector<LoadedMod *> g_mods;
std::vector<Panel> g_panels;
std::mutex g_panels_m;
void (*g_log)(const char *) = nullptr;
std::string g_server;
PowderApi g_api;
thread_local std::string t_err;

std::string narrow(const std::wstring &w) {
    char buf[512];
    WideCharToMultiByte(CP_UTF8, 0, w.c_str(), -1, buf, sizeof(buf), nullptr, nullptr);
    return buf;
}

void logf(const char *fmt, ...) {
    if (!g_log) return;
    char line[600];
    va_list ap; va_start(ap, fmt); vsnprintf(line, sizeof(line), fmt, ap); va_end(ap);
    g_log(line);
}

// [lo, hi) of a module's image (SizeOfImage from its PE header).
bool image_range(const char *module, uintptr_t &lo, uintptr_t &hi) {
    HMODULE m = module ? GetModuleHandleA(module) : GetModuleHandleW(nullptr);
    if (!m) return false;
    auto *dos = (IMAGE_DOS_HEADER *)m;
    auto *nt = (IMAGE_NT_HEADERS64 *)((uint8_t *)m + dos->e_lfanew);
    lo = (uintptr_t)m;
    hi = lo + nt->OptionalHeader.SizeOfImage;
    return true;
}

bool readable(uintptr_t a, size_t n) {
    while (n) {
        MEMORY_BASIC_INFORMATION m;
        if (!VirtualQuery((void *)a, &m, sizeof(m)) || m.State != MEM_COMMIT || (m.Protect & PAGE_GUARD) ||
            (m.Protect & 0xFF) == PAGE_NOACCESS)
            return false;
        uintptr_t end = (uintptr_t)m.BaseAddress + m.RegionSize;
        size_t chunk = end - a < n ? end - a : n;
        a += chunk; n -= chunk;
    }
    return true;
}

// ---- the API ----------------------------------------------------------------------------------------------

void api_log(const PowderMod *mod, const char *fmt, ...) {
    char msg[512];
    va_list ap; va_start(ap, fmt); vsnprintf(msg, sizeof(msg), fmt, ap); va_end(ap);
    logf("[%s] %s", mod && mod->name ? mod->name : "?", msg);
}

uintptr_t api_module_base(const char *module) {
    return (uintptr_t)(module ? GetModuleHandleA(module) : GetModuleHandleW(nullptr));
}

uintptr_t api_find_pattern(const char *module, const char *pattern) {
    std::vector<int> pat;                                    // -1 = wildcard
    for (const char *p = pattern; *p; ) {
        while (*p == ' ') ++p;
        if (!*p) break;
        if (p[0] == '?') { pat.push_back(-1); p += p[1] == '?' ? 2 : 1; continue; }
        char two[3] = {p[0], p[1], 0};
        pat.push_back((int)strtoul(two, nullptr, 16));
        p += 2;
    }
    uintptr_t lo, hi;
    if (pat.empty() || !image_range(module, lo, hi)) return 0;
    for (uintptr_t a = lo; a < hi; ) {
        MEMORY_BASIC_INFORMATION m;
        if (!VirtualQuery((void *)a, &m, sizeof(m))) break;
        uintptr_t end = (uintptr_t)m.BaseAddress + m.RegionSize;
        if (end > hi) end = hi;
        bool ok = m.State == MEM_COMMIT && !(m.Protect & PAGE_GUARD) && (m.Protect & 0xFF) != PAGE_NOACCESS;
        if (ok && end - a >= pat.size()) {
            const uint8_t *b = (const uint8_t *)a;
            size_t n = end - a - pat.size() + 1;
            for (size_t i = 0; i < n; ++i) {
                size_t k = 0;
                while (k < pat.size() && (pat[k] < 0 || b[i + k] == pat[k])) ++k;
                if (k == pat.size()) return a + i;
            }
        }
        a = end;
    }
    return 0;
}

int api_read(uintptr_t addr, void *out, size_t n) {
    if (!readable(addr, n)) return 1;
    memcpy(out, (void *)addr, n);
    return 0;
}

int api_write(uintptr_t addr, const void *src, size_t n) {
    DWORD old;
    if (!VirtualProtect((void *)addr, n, PAGE_READWRITE, &old)) return 1;
    memcpy((void *)addr, src, n);
    DWORD tmp; VirtualProtect((void *)addr, n, old, &tmp);
    return 0;
}

int api_patch_code(uintptr_t addr, const void *bytes, size_t n) {
    std::string how;
    if (write_code(addr, (const uint8_t *)bytes, n, how)) return 0;
    t_err = "could not patch the code (" + how + ")";
    return 1;
}

int api_hook(const PowderMod *mod, uintptr_t target, void *detour, void **original, int *id) {
    int h = hook_function(mod ? mod->name : "?", target, detour, original, t_err);
    if (id) *id = h;
    if (h) logf("[%s] hooked %p", mod ? mod->name : "?", (void *)target);
    else logf("[%s] hook %p failed: %s", mod ? mod->name : "?", (void *)target, t_err.c_str());
    return h ? 0 : 1;
}

int api_hook_mid(const PowderMod *mod, uintptr_t at, PowderMidFn fn, void *user, int *id) {
    int h = hook_mid(mod ? mod->name : "?", at, fn, user, t_err);
    if (id) *id = h;
    if (h) logf("[%s] mid hook at %p", mod ? mod->name : "?", (void *)at);
    else logf("[%s] mid hook %p failed: %s", mod ? mod->name : "?", (void *)at, t_err.c_str());
    return h ? 0 : 1;
}

int api_unhook(int id) { return unhook(id, t_err) ? 0 : 1; }
const char *api_last_error() { return t_err.c_str(); }

void api_add_panel(const PowderMod *mod, const char *title, PowderPanelFn draw, void *user) {
    std::lock_guard<std::mutex> lk(g_panels_m);
    g_panels.push_back({mod, title ? title : "", draw, user});
}

uintptr_t api_player_position() {
    std::string why;
    return find_player_pos(why);
}

std::string where(uintptr_t a) {
    HMODULE m = nullptr;
    char buf[160];
    if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           (LPCSTR)a, &m) && m) {
        char path[MAX_PATH];
        GetModuleFileNameA(m, path, MAX_PATH);
        const char *base = strrchr(path, '\\');
        snprintf(buf, sizeof(buf), "%s+0x%llx", base ? base + 1 : path, (unsigned long long)(a - (uintptr_t)m));
    } else {
        snprintf(buf, sizeof(buf), "%p", (void *)a);
    }
    return buf;
}

} // namespace

void mods_load(const std::wstring &dir, const std::string &server_url, void (*log)(const char *)) {
    g_log = log;
    g_server = server_url;
    memset(&g_api, 0, sizeof(g_api));
    g_api.version = POWDER_API_VERSION;
    g_api.size = sizeof(PowderApi);
    g_api.log = api_log;
    g_api.module_base = api_module_base;
    g_api.find_pattern = api_find_pattern;
    g_api.read = api_read;
    g_api.write = api_write;
    g_api.patch_code = api_patch_code;
    g_api.hook = api_hook;
    g_api.hook_mid = api_hook_mid;
    g_api.unhook = api_unhook;
    g_api.last_error = api_last_error;
    g_api.add_panel = api_add_panel;
    g_api.imgui_context = ImGui::GetCurrentContext();
    void *alloc_user;
    ImGui::GetAllocatorFunctions((ImGuiMemAllocFunc *)&g_api.imgui_alloc, (ImGuiMemFreeFunc *)&g_api.imgui_free,
                                 &alloc_user);
    g_api.player_position = api_player_position;
    g_api.server_url = g_server.c_str();

    std::wstring folder = dir + L"powder_mods\\";
    WIN32_FIND_DATAW fd;
    HANDLE f = FindFirstFileW((folder + L"*.dll").c_str(), &fd);
    if (f == INVALID_HANDLE_VALUE) { logf("mods: none (no %s*.dll)", narrow(folder).c_str()); return; }
    do {
        auto *m = new LoadedMod{};
        m->name = narrow(fd.cFileName);
        m->name = m->name.substr(0, m->name.size() - 4);
        m->mod.name = m->name.c_str();
        m->dll = LoadLibraryW((folder + fd.cFileName).c_str());
        if (!m->dll) {
            m->status = "LoadLibrary failed (" + std::to_string(GetLastError()) + ")";
        } else if (auto init = (PowderModInitFn)GetProcAddress(m->dll, "powder_mod_init")) {
            int rc = init(&g_api, &m->mod);
            m->status = rc == 0 ? "running" : "init returned " + std::to_string(rc);
        } else {
            m->status = "no powder_mod_init export";
        }
        logf("mod %s: %s", m->name.c_str(), m->status.c_str());
        g_mods.push_back(m);
    } while (FindNextFileW(f, &fd));
    FindClose(f);
}

void mods_draw_tab() {
    if (g_mods.empty()) {
        ImGui::TextWrapped("No client mods loaded. powder-wrap loads the ones you name in POWDER_CLIENT_MODS "
                           "(mods/<name>/client/*.dll of this repo, built with native/build.sh mods).");
    }
    for (auto *m : g_mods) ImGui::BulletText("%s - %s", m->name.c_str(), m->status.c_str());

    auto hooks = hook_list();
    if (!hooks.empty() && ImGui::TreeNode("Hooks", "Hooks (%zu)", hooks.size())) {
        for (auto &h : hooks) {
            ImGui::PushID(h.id);
            ImGui::Text("#%d %s %s %s%s", h.id, h.owner.c_str(), h.kind.c_str(), where(h.target).c_str(),
                        h.enabled ? "" : " (removed)");
            if (h.enabled) {
                ImGui::SameLine();
                std::string err;
                if (ImGui::SmallButton("unhook") && !unhook(h.id, err)) logf("unhook #%d: %s", h.id, err.c_str());
            }
            ImGui::PopID();
        }
        ImGui::TreePop();
    }

    std::vector<Panel> panels;
    { std::lock_guard<std::mutex> lk(g_panels_m); panels = g_panels; }
    for (size_t i = 0; i < panels.size(); ++i) {
        const Panel &p = panels[i];
        std::string header = std::string(p.mod && p.mod->name ? p.mod->name : "?") + ": " + p.title + "##" +
                             std::to_string(i);
        if (ImGui::CollapsingHeader(header.c_str(), ImGuiTreeNodeFlags_DefaultOpen)) p.draw(p.user);
    }
}

} // namespace pw
