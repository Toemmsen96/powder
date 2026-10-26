// asset_trace - which game data files Steep reads, where and how much: the runtime half of the asset-loading
// analysis (tools/forge/ maps a .forge offset to its entry). Also the example of a client mod: it hooks ntdll's
// NtCreateFile/NtOpenFile (handle -> path) and NtReadFile (offset, length) through the Powder API, shows
// per-file totals in the overlay's Mods tab, and with "log reads" appends each read of a .forge/.pck/.tbf/.bk2 to
// powder_asset_trace.log beside the overlay: "<ms> <file> <offset> <length>".
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <algorithm>
#include <atomic>
#include <cstdio>
#include <mutex>
#include <string>
#include <unordered_map>
#include <vector>
#include "imgui.h"
#include "powder_api.h"

typedef NTSTATUS(NTAPI *NtCreateFile_t)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, PLARGE_INTEGER,
                                        ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);
typedef NTSTATUS(NTAPI *NtOpenFile_t)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, ULONG, ULONG);
typedef NTSTATUS(NTAPI *NtReadFile_t)(HANDLE, HANDLE, PVOID, PVOID, PIO_STATUS_BLOCK, PVOID, ULONG, PLARGE_INTEGER,
                                      PULONG);
typedef NTSTATUS(NTAPI *NtQueryInformationFile_t)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, ULONG);

static const PowderApi *g_api;
static PowderMod *g_mod;
static NtCreateFile_t o_create;
static NtOpenFile_t o_open;
static NtReadFile_t o_read;
static NtQueryInformationFile_t g_query;

struct FileStat { std::string path, name; uint64_t reads = 0, bytes = 0, opens = 0; };

static std::mutex g_m;
static std::unordered_map<HANDLE, size_t> g_handles;     // handle -> index in g_files (tracked files only)
static std::vector<FileStat> g_files;
static std::unordered_map<std::string, size_t> g_by_path;
static uint64_t g_other_reads = 0, g_other_bytes = 0;
static std::atomic<bool> g_logging{false};
static std::string g_logbuf;
static std::wstring g_logpath;
static thread_local bool t_inside = false;               // our own file work must not be traced

static bool tracked(const std::string &p) {
    std::string l = p;
    std::transform(l.begin(), l.end(), l.begin(), ::tolower);
    for (const char *ext : {".forge", ".pck", ".tbf", ".bk2"})
        if (l.size() > strlen(ext) && l.compare(l.size() - strlen(ext), strlen(ext), ext) == 0) return true;
    return false;
}

static std::string utf8(const UNICODE_STRING *u) {
    if (!u || !u->Buffer) return "";
    int n = WideCharToMultiByte(CP_UTF8, 0, u->Buffer, u->Length / 2, nullptr, 0, nullptr, nullptr);
    std::string s(n, 0);
    WideCharToMultiByte(CP_UTF8, 0, u->Buffer, u->Length / 2, s.data(), n, nullptr, nullptr);
    return s;
}

static void opened(HANDLE h, POBJECT_ATTRIBUTES oa) {
    if (!oa || !oa->ObjectName) return;
    std::string path = utf8(oa->ObjectName);
    bool first = false;
    {
        std::lock_guard<std::mutex> lk(g_m);
        if (!tracked(path)) { g_handles.erase(h); return; }
        auto it = g_by_path.find(path);
        size_t idx;
        if (it == g_by_path.end()) {
            first = true;
            idx = g_files.size();
            FileStat f;
            f.path = path;
            size_t slash = path.find_last_of("\\/");
            f.name = slash == std::string::npos ? path : path.substr(slash + 1);
            g_files.push_back(f);
            g_by_path[path] = idx;
        } else {
            idx = it->second;
        }
        g_files[idx].opens++;
        g_handles[h] = idx;
    }
    if (first) {                    // outside the lock and untraced: the log's own file open comes through here
        t_inside = true;
        g_api->log(g_mod, "first open of %s", path.c_str());
        t_inside = false;
    }
}

static NTSTATUS NTAPI hk_create(PHANDLE h, ACCESS_MASK a, POBJECT_ATTRIBUTES oa, PIO_STATUS_BLOCK io, PLARGE_INTEGER sz,
                                ULONG attr, ULONG share, ULONG disp, ULONG opt, PVOID ea, ULONG eal) {
    NTSTATUS st = o_create(h, a, oa, io, sz, attr, share, disp, opt, ea, eal);
    if (st >= 0 && h && !t_inside) opened(*h, oa);
    return st;
}

static NTSTATUS NTAPI hk_open(PHANDLE h, ACCESS_MASK a, POBJECT_ATTRIBUTES oa, PIO_STATUS_BLOCK io, ULONG share,
                              ULONG opt) {
    NTSTATUS st = o_open(h, a, oa, io, share, opt);
    if (st >= 0 && h && !t_inside) opened(*h, oa);
    return st;
}

static NTSTATUS NTAPI hk_read(HANDLE h, HANDLE ev, PVOID apc, PVOID ctx, PIO_STATUS_BLOCK io, PVOID buf, ULONG len,
                              PLARGE_INTEGER off, PULONG key) {
    if (t_inside) return o_read(h, ev, apc, ctx, io, buf, len, off, key);
    long long pos = -1;
    size_t idx = (size_t)-1;
    {
        std::lock_guard<std::mutex> lk(g_m);
        auto it = g_handles.find(h);
        if (it != g_handles.end()) idx = it->second;
    }
    if (idx != (size_t)-1) {
        if (off && off->QuadPart >= 0) pos = off->QuadPart;
        else if (g_query) {                                  // the file pointer (synchronous reads)
            IO_STATUS_BLOCK qio;
            LARGE_INTEGER cur;
            t_inside = true;
            if (g_query(h, &qio, &cur, sizeof(cur), 14 /* FilePositionInformation */) >= 0) pos = cur.QuadPart;
            t_inside = false;
        }
    }
    NTSTATUS st = o_read(h, ev, apc, ctx, io, buf, len, off, key);
    std::lock_guard<std::mutex> lk(g_m);
    if (idx == (size_t)-1) { g_other_reads++; g_other_bytes += len; return st; }
    FileStat &f = g_files[idx];
    f.reads++;
    f.bytes += len;
    if (g_logging.load() && g_logbuf.size() < (64u << 20)) {
        char line[512];
        snprintf(line, sizeof(line), "%lu %s %lld %lu\n", GetTickCount(), f.name.c_str(), pos, len);
        g_logbuf += line;
    }
    return st;
}

static DWORD WINAPI flusher(LPVOID) {
    for (;;) {
        Sleep(500);
        std::string out;
        { std::lock_guard<std::mutex> lk(g_m); out.swap(g_logbuf); }
        if (out.empty()) continue;
        t_inside = true;
        HANDLE f = CreateFileW(g_logpath.c_str(), FILE_APPEND_DATA, FILE_SHARE_READ, nullptr, OPEN_ALWAYS, 0, nullptr);
        if (f != INVALID_HANDLE_VALUE) {
            DWORD wr;
            WriteFile(f, out.data(), (DWORD)out.size(), &wr, nullptr);
            CloseHandle(f);
        }
        t_inside = false;
    }
}

static void panel(void *) {
    std::vector<FileStat> files;
    uint64_t other_reads, other_bytes;
    {
        std::lock_guard<std::mutex> lk(g_m);
        files = g_files;
        other_reads = g_other_reads; other_bytes = g_other_bytes;
    }
    std::sort(files.begin(), files.end(), [](const FileStat &a, const FileStat &b) { return a.bytes > b.bytes; });
    bool logging = g_logging.load();
    if (ImGui::Checkbox("log reads to powder_asset_trace.log", &logging)) g_logging = logging;
    ImGui::SameLine();
    if (ImGui::SmallButton("reset")) {
        std::lock_guard<std::mutex> lk(g_m);
        for (auto &f : g_files) { f.reads = 0; f.bytes = 0; f.opens = 0; }
        g_other_reads = g_other_bytes = 0;
    }
    ImGui::Text("other files: %llu reads, %.1f MB", (unsigned long long)other_reads, other_bytes / 1048576.0);
    if (ImGui::BeginTable("files", 4, ImGuiTableFlags_Borders | ImGuiTableFlags_RowBg | ImGuiTableFlags_ScrollY,
                          ImVec2(0, 220))) {
        ImGui::TableSetupColumn("File"); ImGui::TableSetupColumn("Opens"); ImGui::TableSetupColumn("Reads");
        ImGui::TableSetupColumn("MB read");
        ImGui::TableHeadersRow();
        for (auto &f : files) {
            ImGui::TableNextRow();
            ImGui::TableSetColumnIndex(0); ImGui::TextUnformatted(f.name.c_str());
            ImGui::TableSetColumnIndex(1); ImGui::Text("%llu", (unsigned long long)f.opens);
            ImGui::TableSetColumnIndex(2); ImGui::Text("%llu", (unsigned long long)f.reads);
            ImGui::TableSetColumnIndex(3); ImGui::Text("%.1f", f.bytes / 1048576.0);
        }
        ImGui::EndTable();
    }
}

extern "C" POWDER_EXPORT int powder_mod_init(const PowderApi *api, PowderMod *mod) {
    if (api->version < 1) return 1;
    g_api = api;
    g_mod = mod;
    if (!powder_imgui_attach(api)) api->log(mod, "ImGui version mismatch: no panel");
    else api->add_panel(mod, "Asset reads", panel, nullptr);

    wchar_t self[MAX_PATH];
    HMODULE me;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                       (LPCWSTR)&powder_mod_init, &me);
    GetModuleFileNameW(me, self, MAX_PATH);
    g_logpath = self;
    g_logpath = g_logpath.substr(0, g_logpath.find_last_of(L'\\'));               // .../powder_mods
    g_logpath = g_logpath.substr(0, g_logpath.find_last_of(L'\\') + 1) + L"powder_asset_trace.log";

    HMODULE nt = GetModuleHandleA("ntdll.dll");
    g_query = (NtQueryInformationFile_t)GetProcAddress(nt, "NtQueryInformationFile");
    int fails = 0;
    fails += api->hook(mod, (uintptr_t)GetProcAddress(nt, "NtCreateFile"), (void *)hk_create, (void **)&o_create, nullptr);
    fails += api->hook(mod, (uintptr_t)GetProcAddress(nt, "NtOpenFile"), (void *)hk_open, (void **)&o_open, nullptr);
    fails += api->hook(mod, (uintptr_t)GetProcAddress(nt, "NtReadFile"), (void *)hk_read, (void **)&o_read, nullptr);
    CreateThread(nullptr, 0, flusher, nullptr, 0, nullptr);
    api->log(mod, "tracing file reads (%d hook(s) failed)", fails);
    return 0;
}
