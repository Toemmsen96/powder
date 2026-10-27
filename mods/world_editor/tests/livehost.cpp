// livehost - a stand-in for the overlay's mod host, to test world_editor.dll's live edits under Wine without the
// game (run by test_live.py --wine). It gives the DLL the Powder API (hooks through MinHook, as the overlay does),
// lets it load the live helper's manifest, and then reads every served cell's entry from its archive the way a game
// might: synchronous with the file pointer, overlapped with an event, overlapped on a completion port bound before
// the DLL came (the port must be learnt) and after it, with FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, through
// GetQueuedCompletionStatusEx, and ReadFileEx with an APC. Each mode's bytes go to <out>/<mode>/<id>.bin.
//
// "grow" cells: the host holds the archive's index like Steep does (an array of 24-byte {u64 offset, u64 id, u32
// size, u32} records, from <records file>), waits for the DLL to point the entry's record past the end of the file,
// and reads where the record then says.
//
//     livehost <world_editor.dll> <game dir> <records file> <out dir>
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cstdarg>
#include <cstdio>
#include <fstream>
#include <map>
#include <sstream>
#include <string>
#include <vector>
#include "MinHook.h"
#include "imgui.h"
#include "powder_api.h"

static std::atomic<int> g_gen{-1};
static std::string g_err;
static int fails = 0;
#define CHECK(c, ...) do { if (!(c)) { printf("FAIL %d: %s ", __LINE__, #c); printf(__VA_ARGS__); printf("\n"); \
                                       fflush(stdout); ++fails; } } while (0)

static void api_log(const PowderMod *mod, const char *fmt, ...) {
    char line[2048];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    printf("[%s] %s\n", mod ? mod->name : "?", line);
    fflush(stdout);
    const char *g = strstr(line, "live: generation ");
    if (g) g_gen = atoi(g + 17);
}
static uintptr_t api_module_base(const char *m) { return (uintptr_t)GetModuleHandleA(m); }
static uintptr_t api_find_pattern(const char *, const char *) { return 0; }
static int api_read(uintptr_t a, void *out, size_t n) {
    SIZE_T got = 0;
    return ReadProcessMemory(GetCurrentProcess(), (void *)a, out, n, &got) && got == n ? 0 : 1;
}
static int api_write(uintptr_t a, const void *src, size_t n) {
    DWORD old, tmp;
    if (!VirtualProtect((void *)a, n, PAGE_READWRITE, &old)) return 1;
    memcpy((void *)a, src, n);
    VirtualProtect((void *)a, n, old, &tmp);
    return 0;
}
static int api_patch_code(uintptr_t, const void *, size_t) { g_err = "no code patches here"; return 1; }
static int api_hook(const PowderMod *, uintptr_t target, void *detour, void **orig, int *id) {
    MH_STATUS s = MH_CreateHook((void *)target, detour, orig);
    if (s == MH_OK) s = MH_EnableHook((void *)target);
    if (s != MH_OK) { g_err = MH_StatusToString(s); return 1; }
    if (id) *id = 1;
    return 0;
}
static int api_hook_mid(const PowderMod *, uintptr_t, PowderMidFn, void *, int *) { g_err = "no"; return 1; }
static int api_unhook(int) { return 1; }
static const char *api_last_error() { return g_err.c_str(); }
static void api_add_panel(const PowderMod *, const char *, PowderPanelFn, void *) {}
static void *api_alloc(size_t n, void *) { return malloc(n); }
static void api_free(void *p, void *) { free(p); }
static uintptr_t api_player_position() { return 0; }
static void api_on_frame(const PowderMod *, PowderFrameFn, void *) {}
static int api_http(const PowderMod *, const char *, const char *, const char *, PowderHttpFn, void *) {
    g_err = "no server here";
    return 1;
}

struct Cell { std::string archive, mode, id; uint64_t off = 0, size = 0, bytes = 0; uint32_t index = 0; };

static std::vector<std::string> split(const std::string &s, char c) {
    std::vector<std::string> out;
    std::stringstream ss(s);
    std::string x;
    while (std::getline(ss, x, c)) out.push_back(x);
    return out;
}

static std::wstring W(const std::string &s) { return std::wstring(s.begin(), s.end()); }

enum Mode { SYNC, EVENT, PORT_LATE, PORT_LATE_EX, PORT_LATE_SKIP, PORT, PORT_SKIP, APC, NMODES };
static const char *MODE_NAMES[] = {"sync", "event", "port-late", "port-late-ex", "port-late-skip", "port", "port-skip",
                                     "apc"};

struct Reader {
    Mode mode;
    HANDLE f = INVALID_HANDLE_VALUE, port = nullptr;
    bool skip = false;
    int packets = 0;
};

static HANDLE open_archive(const std::string &path, bool overlapped) {
    return CreateFileW(W(path).c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING,
                       overlapped ? FILE_FLAG_OVERLAPPED : 0, nullptr);
}

static void bind(Reader &r, bool skip) {
    r.port = CreateIoCompletionPort(r.f, nullptr, 0x5eed, 1);
    if (skip) {
        SetFileCompletionNotificationModes(r.f, FILE_SKIP_COMPLETION_PORT_ON_SUCCESS);
        r.skip = true;
    }
}

static DWORD g_apc_got;
static void CALLBACK apc_done(DWORD err, DWORD got, LPOVERLAPPED) { g_apc_got = err ? 0 : got; }

// One aligned read of n bytes at pos into buf, as the mode does it. Bytes read.
static DWORD read_at(Reader &r, uint64_t pos, uint8_t *buf, DWORD n) {
    OVERLAPPED ov;
    memset(&ov, 0, sizeof ov);
    ov.Offset = (DWORD)pos;
    ov.OffsetHigh = (DWORD)(pos >> 32);
    DWORD got = 0;
    switch (r.mode) {
    case SYNC: {
        LARGE_INTEGER li;
        li.QuadPart = (LONGLONG)pos;
        SetFilePointerEx(r.f, li, nullptr, FILE_BEGIN);
        if (!ReadFile(r.f, buf, n, &got, nullptr)) return 0;
        return got;
    }
    case EVENT: {
        ov.hEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!ReadFile(r.f, buf, n, nullptr, &ov) && GetLastError() != ERROR_IO_PENDING) got = 0;
        else GetOverlappedResult(r.f, &ov, &got, TRUE);
        CloseHandle(ov.hEvent);
        return got;
    }
    case APC: {
        g_apc_got = (DWORD)-1;
        if (!ReadFileEx(r.f, buf, n, &ov, apc_done)) return 0;
        while (g_apc_got == (DWORD)-1) SleepEx(1000, TRUE);
        return g_apc_got;
    }
    default: {
        BOOL now = ReadFile(r.f, buf, n, nullptr, &ov);
        if (!now && GetLastError() != ERROR_IO_PENDING) return 0;
        if (now && r.skip) return (DWORD)ov.InternalHigh;          // completed: no packet comes
        ULONG_PTR key = 0;
        OVERLAPPED *done = nullptr;
        if (r.mode == PORT_LATE_EX) {
            OVERLAPPED_ENTRY e[4];
            ULONG k = 0;
            if (!GetQueuedCompletionStatusEx(r.port, e, 4, &k, 10000, FALSE) || k != 1) return 0;
            key = e[0].lpCompletionKey;
            done = e[0].lpOverlapped;
            got = e[0].dwNumberOfBytesTransferred;
        } else if (!GetQueuedCompletionStatus(r.port, &got, &key, &done, 10000)) {
            return 0;
        }
        r.packets++;
        CHECK(done == &ov && key == 0x5eed, "packet for another read (mode %s)", MODE_NAMES[r.mode]);
        return got;
    }
    }
}

int main(int argc, char **argv) {
    if (argc < 5) { printf("usage: livehost <dll> <game dir> <records file> <out dir>\n"); return 2; }
    std::string dll = argv[1], game = argv[2], recfile = argv[3], out = argv[4];
    setvbuf(stdout, nullptr, _IONBF, 0);
    MH_Initialize();
    ImGui::CreateContext();
    // the archive's index, held like the game holds it: 24-byte records (u32 of junk after each)
    std::map<std::string, std::vector<uint8_t>> arrays;
    {
        std::ifstream rf(recfile);
        std::string line;
        while (std::getline(rf, line)) {
            auto c = split(line, '\t');
            if (c.size() < 2) continue;
            auto &arr = arrays[c[0]];
            for (size_t i = 1; i < c.size(); ++i) {
                auto v = split(c[i], ',');
                uint64_t off = strtoull(v[0].c_str(), nullptr, 10), id = strtoull(v[1].c_str(), nullptr, 16);
                uint32_t size = (uint32_t)strtoul(v[2].c_str(), nullptr, 10), junk = 0xC7000001u * (uint32_t)i;
                uint8_t rec[24];
                memcpy(rec, &off, 8); memcpy(rec + 8, &id, 8); memcpy(rec + 16, &size, 4); memcpy(rec + 20, &junk, 4);
                arr.insert(arr.end(), rec, rec + 24);
            }
            printf("host: %s index of %zu records at %p\n", c[0].c_str(), arr.size() / 24, (void *)arr.data());
        }
    }
    std::vector<Cell> cells;
    {
        std::ifstream mf(game + "/powder_mods/world_editor/live/manifest.tsv");
        std::string line;
        while (std::getline(mf, line)) {
            auto c = split(line, '\t');
            if (c.size() < 11 || c[0] != "cell") continue;
            Cell x;
            x.archive = c[1];
            x.index = (uint32_t)strtoul(c[2].c_str(), nullptr, 10);
            auto rec = split(c[3], ',');
            x.off = strtoull(rec[0].c_str(), nullptr, 10);
            x.id = rec[1];
            x.size = strtoull(rec[2].c_str(), nullptr, 10);
            x.mode = c[6];
            x.bytes = strtoull(c[8].c_str(), nullptr, 10);
            cells.push_back(x);
        }
    }
    printf("host: %zu served cell(s) in the manifest\n", cells.size());
    std::map<std::string, Reader> late, late_ex, late_skip;   // bound to their port before the DLL is there
    for (auto &c : cells) {
        for (auto *m : {&late, &late_ex, &late_skip}) {
            if (m->count(c.archive)) continue;
            Reader r;
            r.mode = m == &late ? PORT_LATE : m == &late_ex ? PORT_LATE_EX : PORT_LATE_SKIP;
            r.f = open_archive(game + "/" + c.archive, true);
            bind(r, m == &late_skip);
            (*m)[c.archive] = r;
        }
    }

    static PowderApi api;
    memset(&api, 0, sizeof api);
    api.version = POWDER_API_VERSION;
    api.size = sizeof api;
    api.log = api_log;
    api.module_base = api_module_base;
    api.find_pattern = api_find_pattern;
    api.read = api_read;
    api.write = api_write;
    api.patch_code = api_patch_code;
    api.hook = api_hook;
    api.hook_mid = api_hook_mid;
    api.unhook = api_unhook;
    api.last_error = api_last_error;
    api.add_panel = api_add_panel;
    api.imgui_context = ImGui::GetCurrentContext();
    api.imgui_alloc = api_alloc;
    api.imgui_free = api_free;
    api.player_position = api_player_position;
    api.server_url = "https://public-ubiservices.ubi.com";
    api.on_frame = api_on_frame;
    api.http = api_http;
    static PowderMod mod = {"world_editor", nullptr};
    HMODULE h = LoadLibraryW(W(dll).c_str());
    CHECK(h, "LoadLibrary %s: %lu", dll.c_str(), GetLastError());
    if (!h) return 1;
    auto init = (PowderModInitFn)GetProcAddress(h, "powder_mod_init");
    CHECK(init && init(&api, &mod) == 0, "init");
    for (int i = 0; i < 300 && g_gen < 0; ++i) Sleep(100);
    CHECK(g_gen >= 0, "the DLL loaded no manifest");

    // grow cells: wait for the DLL to redirect the record, then read where it points
    std::map<std::string, std::pair<uint64_t, uint64_t>> where;   // id -> (offset, size) to read
    for (auto &c : cells) {
        if (c.mode == "same") { where[c.id] = {c.off, c.size}; continue; }
        auto it = arrays.find(c.archive);
        CHECK(it != arrays.end(), "no index for %s", c.archive.c_str());
        if (it == arrays.end()) continue;
        uint8_t *rec = it->second.data() + 24ull * c.index;
        uint64_t off = 0;
        uint32_t size = 0;
        for (int i = 0; i < 600; ++i) {
            memcpy(&off, rec, 8);
            memcpy(&size, rec + 16, 4);
            if (off != c.off) break;
            Sleep(100);
        }
        uint32_t junk;
        memcpy(&junk, rec + 20, 4);
        CHECK(off != c.off && size == c.bytes, "%s: record not redirected (off %llu size %u)", c.id.c_str(),
              (unsigned long long)off, size);
        CHECK(junk == 0xC7000001u * (c.index + 1), "the record's padding changed");
        printf("host: %s record -> offset %llu size %u\n", c.id.c_str(), (unsigned long long)off, size);
        where[c.id] = {off, size};
    }

    const DWORD sizes[] = {4096, 24576, 106496, 131072, 8192, 655360};
    for (int pass = 0; pass < 2; ++pass) {                  // pass 0: the late ports are not learnt yet
        for (int m = 0; m < NMODES; ++m) {
            if (pass == 1 && m != PORT_LATE && m != PORT_LATE_EX && m != PORT_LATE_SKIP) continue;
            std::string dir = out + "/" + MODE_NAMES[m] + (pass ? "-2" : "");
            CreateDirectoryW(W(dir).c_str(), nullptr);
            std::map<std::string, Reader> readers;
            for (auto &c : cells) {
                std::string id = c.id;
                Reader *r;
                if (m == PORT_LATE) r = &late[c.archive];
                else if (m == PORT_LATE_EX) r = &late_ex[c.archive];
                else if (m == PORT_LATE_SKIP) r = &late_skip[c.archive];
                else {
                    if (!readers.count(c.archive)) {
                        Reader nr;
                        nr.mode = (Mode)m;
                        nr.f = open_archive(game + "/" + c.archive, m != SYNC);
                        if (m == PORT || m == PORT_SKIP) bind(nr, m == PORT_SKIP);
                        readers[c.archive] = nr;
                    }
                    r = &readers[c.archive];
                }
                uint64_t start = where[id].first, want = where[id].second;
                std::vector<uint8_t> entry(want, 0);
                uint64_t pos = start & ~4095ull;
                int k = 0;
                bool ok = true;
                while (pos < start + want) {
                    DWORD n = sizes[k++ % 6];
                    std::vector<uint8_t> buf(n, 0xEE);
                    DWORD got = read_at(*r, pos, buf.data(), n);
                    if (!got) { ok = false; break; }
                    for (uint64_t j = 0; j < got; ++j)
                        if (pos + j >= start && pos + j < start + want) entry[pos + j - start] = buf[j];
                    pos += n;
                }
                CHECK(ok, "mode %s: a read of %s failed (%lu)", MODE_NAMES[m], id.c_str(), GetLastError());
                std::FILE *o = std::fopen((dir + "/" + id + ".bin").c_str(), "wb");
                if (o) { std::fwrite(entry.data(), 1, entry.size(), o); std::fclose(o); }
            }
            // no packet twice, none left over
            auto &used = m == PORT_LATE ? late : m == PORT_LATE_EX ? late_ex
                        : m == PORT_LATE_SKIP ? late_skip : readers;
            for (auto &[a, r] : used) {
                if (!r.port) continue;
                DWORD got;
                ULONG_PTR key;
                OVERLAPPED *ov = nullptr;
                BOOL extra = GetQueuedCompletionStatus(r.port, &got, &key, &ov, 300);
                CHECK(!extra && !ov, "mode %s: a packet left over on %s", MODE_NAMES[m], a.c_str());
            }
            for (auto &[a, r] : readers) { CloseHandle(r.f); if (r.port) CloseHandle(r.port); }
            printf("host: mode %s%s done\n", MODE_NAMES[m], pass ? " (learnt)" : "");
        }
    }
    // phase 2 (if <out>/next appears): the helper writes another generation; grown cells it no longer serves must
    // get their own records back, and every cell reads as served (or as the archive has it) again
    printf("host: phase 1 %s; waiting for the next generation\n", fails ? "FAILED" : "ok");
    int gen1 = g_gen;
    bool next = false;
    for (int i = 0; i < 1200 && !next; ++i) {
        next = GetFileAttributesW(W(out + "/next").c_str()) != INVALID_FILE_ATTRIBUTES;
        if (!next) Sleep(100);
    }
    if (next) {
        for (int i = 0; i < 300 && g_gen == gen1; ++i) Sleep(100);
        CHECK(g_gen > gen1, "no second generation loaded");
        Sleep(500);                                       // the records are written right after the log line
        std::map<std::string, Cell> now;
        std::ifstream mf(game + "/powder_mods/world_editor/live/manifest.tsv");
        std::string line;
        while (std::getline(mf, line)) {
            auto c = split(line, '\t');
            if (c.size() >= 11 && c[0] == "cell") now[split(c[3], ',')[1]] = Cell{c[1], c[6], split(c[3], ',')[1]};
        }
        std::string dir = out + "/phase2";
        CreateDirectoryW(W(dir).c_str(), nullptr);
        for (auto &c : cells) {
            uint64_t off = c.off, size = c.size;
            if (c.mode == "grow") {
                uint8_t *rec = arrays[c.archive].data() + 24ull * c.index;
                uint32_t sz;
                memcpy(&off, rec, 8);
                memcpy(&sz, rec + 16, 4);
                size = sz;
                bool still = now.count(c.id) && now[c.id].mode == "grow";
                CHECK(still ? off != c.off : off == c.off && size == c.size, "%s: record %s", c.id.c_str(),
                      still ? "not redirected" : "not restored");
                printf("host: phase 2: %s record %s (offset %llu size %llu)\n", c.id.c_str(),
                       still ? "still redirected" : "restored", (unsigned long long)off, (unsigned long long)size);
            }
            Reader r;
            r.mode = PORT;
            r.f = open_archive(game + "/" + c.archive, true);
            bind(r, false);
            std::vector<uint8_t> entry(size, 0);
            uint64_t pos = off & ~4095ull;
            int k = 0;
            while (pos < off + size) {
                DWORD n = sizes[k++ % 6];
                std::vector<uint8_t> buf(n, 0xEE);
                DWORD got = read_at(r, pos, buf.data(), n);
                CHECK(got, "phase 2: a read failed");
                if (!got) break;
                for (uint64_t j = 0; j < got; ++j)
                    if (pos + j >= off && pos + j < off + size) entry[pos + j - off] = buf[j];
                pos += n;
            }
            CloseHandle(r.f);
            CloseHandle(r.port);
            std::FILE *o = std::fopen((dir + "/" + c.id + ".bin").c_str(), "wb");
            if (o) { std::fwrite(entry.data(), 1, entry.size(), o); std::fclose(o); }
        }
    }
    printf("host: %s\n", fails ? "FAILED" : "ok");
    fflush(stdout);
    ExitProcess(fails ? 1 : 0);
}
