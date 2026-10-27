// live.cpp - live world edits: edited world cells served in place of the archives' bytes as Steep reads them, so
// an edit shows the next time the game streams its cell in, without a restart. The session helper (live.py) turns
// the server's edit list into complete cell datafiles and lists them in <dir>live\manifest.tsv, polled here.
//
//  - "same": the datafile was fitted to exactly its entry's size (forge.py pad_datafile): every read overlapping
//    the entry's range gets its bytes. Nothing in the game's memory changes.
//  - "grow": the datafile is longer than the entry. Each archive's index lives in the game as one array of
//    {u64 offset, u64 id, u32 size, u32} records, all tables in file order (the only place the game keeps an
//    entry's offset; tools/forge/WORLD.md "The archive index in memory"): the entry's record is pointed at a slot
//    past the end of the file,
//    whose reads are answered from here. Undone, the record gets its own values back.
//
// Reads come through ntdll's NtReadFile. A read that touches served bytes is done privately (own event, no APC, no
// completion packet), patched, and then completed for the caller the way the I/O manager would (its event, its
// APC, or a packet on the file's completion port), so the game never sees the buffer before it is patched. A
// handle's completion port is seen when it is bound (NtSetInformationFile) or learnt from the completions the game
// dequeues (NtRemoveIoCompletion[Ex]); until then a patched read without an event or APC completes as the game
// asked and is patched at the first of "the read returned" and "its packet was dequeued". The hooks are installed
// once a manifest exists (the helper runs); they do nothing to files other than .forge archives.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>
#include <algorithm>
#include <atomic>
#include <cstdio>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <string>
#include <unordered_map>
#include <vector>
#include "live.h"
#include "live_patch.h"

namespace live {
namespace {

typedef NTSTATUS(NTAPI *NtCreateFile_t)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, PLARGE_INTEGER,
                                        ULONG, ULONG, ULONG, ULONG, PVOID, ULONG);
typedef NTSTATUS(NTAPI *NtOpenFile_t)(PHANDLE, ACCESS_MASK, POBJECT_ATTRIBUTES, PIO_STATUS_BLOCK, ULONG, ULONG);
typedef NTSTATUS(NTAPI *NtReadFile_t)(HANDLE, HANDLE, PIO_APC_ROUTINE, PVOID, PIO_STATUS_BLOCK, PVOID, ULONG,
                                      PLARGE_INTEGER, PULONG);
typedef NTSTATUS(NTAPI *NtSetInformationFile_t)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, ULONG);
typedef NTSTATUS(NTAPI *NtQueryInformationFile_t)(HANDLE, PIO_STATUS_BLOCK, PVOID, ULONG, ULONG);
typedef NTSTATUS(NTAPI *NtRemoveIoCompletion_t)(HANDLE, PULONG_PTR, PULONG_PTR, PIO_STATUS_BLOCK, PLARGE_INTEGER);
struct IoCompletionInfo { ULONG_PTR key, value; IO_STATUS_BLOCK iosb; };   // FILE_IO_COMPLETION_INFORMATION
typedef NTSTATUS(NTAPI *NtRemoveIoCompletionEx_t)(HANDLE, IoCompletionInfo *, ULONG, PULONG, PLARGE_INTEGER, BOOLEAN);
typedef NTSTATUS(NTAPI *NtSetIoCompletion_t)(HANDLE, ULONG_PTR, ULONG_PTR, NTSTATUS, ULONG_PTR);
typedef NTSTATUS(NTAPI *NtQueueApcThread_t)(HANDLE, PVOID, ULONG_PTR, ULONG_PTR, ULONG_PTR);

constexpr NTSTATUS ST_PENDING = 0x103;
constexpr ULONG FILE_NAME_INFO_CLASS = 9, FILE_POSITION_INFO_CLASS = 14, FILE_COMPLETION_INFO_CLASS = 30;

const PowderApi *g_api;
PowderMod *g_mod;
std::wstring g_dir, g_game;
NtCreateFile_t o_create;
NtOpenFile_t o_open;
NtReadFile_t o_read;
NtSetInformationFile_t o_setinfo;
NtRemoveIoCompletion_t o_remove;
NtRemoveIoCompletionEx_t o_remove_ex;
NtQueryInformationFile_t g_query;
NtSetInformationFile_t g_setinfo;
NtSetIoCompletion_t g_post;
NtQueueApcThread_t g_queue_apc;
thread_local bool t_inside = false;                    // our own file work (and logging) bypasses the hooks

template <class... A> void logf(const char *fmt, A... a) {
    bool was = t_inside;
    t_inside = true;
    g_api->log(g_mod, fmt, a...);
    t_inside = was;
}

// ---- what is served (built by the loader thread, published whole; the hooks take a snapshot) ------------------
struct FileTable {
    std::vector<Range> ranges;
    std::vector<std::shared_ptr<std::atomic<uint64_t>>> reads;   // per range: patched reads (the panel's "read N")
    std::vector<std::shared_ptr<std::vector<uint8_t>>> owners;   // per range: the bytes it points into
    std::vector<bool> carried;                                   // per range: kept over from the generation before
    uint64_t vbase = UINT64_MAX;                                 // the virtual area (grow mode), if any
};
struct Table {
    std::unordered_map<std::string, FileTable> files;            // archive file name, lower case
};
std::shared_ptr<const Table> g_table = std::make_shared<Table>();
std::atomic<bool> g_any{false};

std::shared_ptr<const Table> snapshot() { return std::atomic_load(&g_table); }

// ---- handles: which archive, and how its reads complete --------------------------------------------------------
enum : int8_t { PORT_UNKNOWN, PORT_BOUND, PORT_NONE };
struct HInfo {
    bool named = false;
    const std::string *file = nullptr;                 // an archive (.forge), else null
    int8_t port = PORT_UNKNOWN;
    HANDLE port_h = nullptr;
    ULONG_PTR key = 0;
};
struct Pending {                                       // a patched read completing normally (port not known yet)
    std::mutex m;
    bool done = false;
    uint8_t *buf = nullptr;
    size_t n = 0;
    uint64_t pos = 0;
    bool virt = false;
    std::shared_ptr<const Table> t;
    const FileTable *ft = nullptr;
};
std::mutex g_hm;                                       // never held while calling the game's ntdll or logging
std::unordered_map<HANDLE, HInfo> g_h;
std::set<std::string> g_names;                         // interned archive names (stable addresses)
std::unordered_map<ULONG_PTR, HANDLE> g_await;         // a read's ApcContext -> its handle (to learn the port)
std::unordered_map<ULONG_PTR, std::shared_ptr<Pending>> g_pending;
std::vector<std::string> g_io_notes;                   // how archive reads complete (logged once each)

std::string lower_base(const wchar_t *p, size_t n) {
    size_t s = 0;
    for (size_t i = 0; i < n; ++i)
        if (p[i] == L'\\' || p[i] == L'/') s = i + 1;
    std::string out;
    for (size_t i = s; i < n; ++i) out += (char)(p[i] < 128 ? tolower((int)p[i]) : '?');
    return out;
}
bool is_archive(const std::string &name) {
    return name.size() > 6 && name.compare(name.size() - 6, 6, ".forge") == 0;
}
void set_name(HInfo &hi, const std::string &name) {    // under g_hm
    hi.named = true;
    hi.file = is_archive(name) ? &*g_names.insert(name).first : nullptr;
}

void opened(HANDLE h, POBJECT_ATTRIBUTES oa) {
    if (!oa || !oa->ObjectName || !oa->ObjectName->Buffer) return;
    std::string name = lower_base(oa->ObjectName->Buffer, oa->ObjectName->Length / 2);
    std::lock_guard<std::mutex> lk(g_hm);
    HInfo hi;                                          // a reused handle value starts afresh
    set_name(hi, name);
    g_h[h] = hi;
}

NTSTATUS NTAPI hk_create(PHANDLE h, ACCESS_MASK a, POBJECT_ATTRIBUTES oa, PIO_STATUS_BLOCK io, PLARGE_INTEGER sz,
                         ULONG attr, ULONG share, ULONG disp, ULONG opt, PVOID ea, ULONG eal) {
    NTSTATUS st = o_create(h, a, oa, io, sz, attr, share, disp, opt, ea, eal);
    if (st >= 0 && h && !t_inside) opened(*h, oa);
    return st;
}

NTSTATUS NTAPI hk_open(PHANDLE h, ACCESS_MASK a, POBJECT_ATTRIBUTES oa, PIO_STATUS_BLOCK io, ULONG share, ULONG opt) {
    NTSTATUS st = o_open(h, a, oa, io, share, opt);
    if (st >= 0 && h && !t_inside) opened(*h, oa);
    return st;
}

// The handle's state; a handle opened before the hooks (the game opens its archives at start) is named once.
bool handle_info(HANDLE h, HInfo &out) {
    {
        std::lock_guard<std::mutex> lk(g_hm);
        auto it = g_h.find(h);
        if (it != g_h.end() && it->second.named) { out = it->second; return out.file != nullptr; }
    }
    std::string name;
    if (g_query) {
        alignas(8) uint8_t buf[4 + 2 * 1024];
        IO_STATUS_BLOCK io;
        t_inside = true;
        NTSTATUS st = g_query(h, &io, buf, sizeof buf - 2, FILE_NAME_INFO_CLASS);
        t_inside = false;
        if (st >= 0) {
            ULONG len = *(ULONG *)buf;
            if (len <= sizeof buf - 6) name = lower_base((const wchar_t *)(buf + 4), len / 2);
        }
    }
    std::lock_guard<std::mutex> lk(g_hm);
    HInfo &hi = g_h[h];
    set_name(hi, name);
    out = hi;
    return out.file != nullptr;
}

void note_io(const std::string &line) {
    {
        std::lock_guard<std::mutex> lk(g_hm);
        for (auto &n : g_io_notes) if (n == line) return;
        if (g_io_notes.size() > 32) return;
        g_io_notes.push_back(line);
    }
    logf("live: %s", line.c_str());
}

// A read whose completion may come through a port: if its context comes out of NtRemoveIoCompletion[Ex], the
// handle is bound to that port. (Never guessed the other way: a packet the game waits for and does not get would
// hang its read; an unknown handle's patched reads complete as the game asked instead.)
void learn_read(HANDLE h, ULONG_PTR ctx) {
    std::lock_guard<std::mutex> lk(g_hm);
    auto it = g_h.find(h);
    if (it == g_h.end() || it->second.port != PORT_UNKNOWN) return;
    if (g_await.size() > 4096) g_await.clear();
    g_await[ctx] = h;
}

void count_reads(const FileTable &ft, uint64_t pos, size_t n) {
    for (size_t i = 0; i < ft.ranges.size(); ++i)
        if (overlaps(ft.ranges[i], pos, n)) ft.reads[i]->fetch_add(1);
}

void complete_pending(Pending &p, size_t got) {
    std::lock_guard<std::mutex> lk(p.m);
    if (p.done) return;
    p.done = true;
    if (p.virt) virtual_read(p.ft->ranges, p.pos, p.buf, p.n);
    else overlay(p.ft->ranges, p.pos, p.buf, got < p.n ? got : p.n);
    count_reads(*p.ft, p.pos, p.n);
}

HANDLE thread_event() {
    thread_local HANDLE ev = nullptr;
    if (!ev) ev = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    return ev;
}

NTSTATUS NTAPI hk_read(HANDLE h, HANDLE ev, PIO_APC_ROUTINE apc, PVOID ctx, PIO_STATUS_BLOCK io, PVOID buf, ULONG len,
                       PLARGE_INTEGER off, PULONG key) {
    if (t_inside || !io || !buf || !len) return o_read(h, ev, apc, ctx, io, buf, len, off, key);
    HInfo hi;
    if (!handle_info(h, hi)) return o_read(h, ev, apc, ctx, io, buf, len, off, key);
    if (hi.port == PORT_UNKNOWN && ctx && !apc) learn_read(h, (ULONG_PTR)ctx);
    if (!g_any.load(std::memory_order_acquire)) return o_read(h, ev, apc, ctx, io, buf, len, off, key);
    auto t = snapshot();
    auto it = t->files.find(*hi.file);
    if (it == t->files.end()) return o_read(h, ev, apc, ctx, io, buf, len, off, key);
    const FileTable &ft = it->second;
    uint64_t pos;
    bool explicit_pos = off && off->QuadPart >= 0;
    if (explicit_pos) {
        pos = (uint64_t)off->QuadPart;
    } else {                                           // the file pointer (synchronous handles)
        IO_STATUS_BLOCK qio;
        LARGE_INTEGER cur;
        t_inside = true;
        NTSTATUS q = g_query ? g_query(h, &qio, &cur, sizeof cur, FILE_POSITION_INFO_CLASS) : -1;
        t_inside = false;
        if (q < 0) return o_read(h, ev, apc, ctx, io, buf, len, off, key);
        pos = (uint64_t)cur.QuadPart;
    }
    bool virt = pos >= ft.vbase;
    if (!virt && !any_overlap(ft.ranges, pos, len)) return o_read(h, ev, apc, ctx, io, buf, len, off, key);

    // A read with an event and no APC is a plain overlapped read (an IOCP'd handle's would come without one, or with
    // the event's low bit set, which makes kernel32 pass no context): it is completed privately like the others.
    bool deferred = ctx && !apc && !ev && hi.port == PORT_UNKNOWN;
    char how[200];
    snprintf(how, sizeof how, "%s: patched reads complete by %s (event %s, APC %s, context %s)", hi.file->c_str(),
             apc ? "APC" : deferred ? "the game's own completion (port not known yet)"
                 : ctx && hi.port == PORT_BOUND ? "a packet on its completion port" : "return/event",
             ev ? "yes" : "no", apc ? "yes" : "no", ctx ? "yes" : "no");
    note_io(how);

    if (deferred) {                                    // completes as the game asked; patched on return or dequeue
        // (Wine answers STATUS_PENDING for overlapped files even when the read is done; the packet patches then)
        auto p = std::make_shared<Pending>();
        p->buf = (uint8_t *)buf; p->n = len; p->pos = pos; p->virt = virt; p->t = t; p->ft = &ft;
        {
            std::lock_guard<std::mutex> lk(g_hm);
            if (g_pending.size() > 1024)
                for (auto i = g_pending.begin(); i != g_pending.end();) i = i->second->done ? g_pending.erase(i) : ++i;
            g_pending[(ULONG_PTR)ctx] = p;
        }
        LARGE_INTEGER zero;
        zero.QuadPart = 0;                             // a virtual read: the same length of real bytes carries it
        NTSTATUS st = o_read(h, ev, apc, ctx, io, buf, len, virt ? &zero : off, key);
        if (st >= 0 && st != ST_PENDING) complete_pending(*p, io->Information);
        return st;
    }

    IO_STATUS_BLOCK my;
    memset(&my, 0, sizeof my);
    NTSTATUS st = 0;
    if (virt) {
        my.Information = virtual_read(ft.ranges, pos, (uint8_t *)buf, len);
        if (!explicit_pos) {                           // a read at the file pointer moves it on
            LARGE_INTEGER np;
            np.QuadPart = (LONGLONG)(pos + len);
            IO_STATUS_BLOCK sio;
            t_inside = true;
            g_setinfo(h, &sio, &np, sizeof np, FILE_POSITION_INFO_CLASS);
            t_inside = false;
        }
    } else {
        HANDLE e = thread_event();
        st = o_read(h, e, nullptr, nullptr, &my, buf, len, off, key);
        if (st == ST_PENDING) {
            WaitForSingleObject(e, INFINITE);
            st = my.Status;
        }
        if (st >= 0) overlay(ft.ranges, pos, (uint8_t *)buf, my.Information < len ? my.Information : len);
    }
    if (st >= 0) count_reads(ft, pos, len);
    io->Status = st;
    io->Information = my.Information;
    if (st < 0) return st;                             // a failure completes nothing (as when it fails at once)
    if (ev) SetEvent(ev);
    if (apc) {
        g_queue_apc(GetCurrentThread(), (PVOID)apc, (ULONG_PTR)ctx, (ULONG_PTR)io, 0);
    } else if (ctx && hi.port == PORT_BOUND && hi.port_h) {
        // The packet is the completion. Answered "pending" (as Wine answers overlapped reads anyway), so that a game
        // with FILE_SKIP_COMPLETION_PORT_ON_SUCCESS (maybe set before the hooks) does not also complete it inline.
        g_post(hi.port_h, hi.key, (ULONG_PTR)ctx, st, my.Information);
        return ST_PENDING;
    }
    return st;
}

NTSTATUS NTAPI hk_setinfo(HANDLE h, PIO_STATUS_BLOCK io, PVOID info, ULONG len, ULONG cls) {
    NTSTATUS st = o_setinfo(h, io, info, len, cls);
    if (st >= 0 && info && !t_inside && cls == FILE_COMPLETION_INFO_CLASS && len >= 2 * sizeof(void *)) {
        std::lock_guard<std::mutex> lk(g_hm);        // the handle is bound to a completion port
        HInfo &hi = g_h[h];
        hi.port_h = ((HANDLE *)info)[0];
        hi.key = ((ULONG_PTR *)info)[1];
        hi.port = hi.port_h ? PORT_BOUND : PORT_NONE;
    }
    return st;
}

void dequeued(HANDLE port, ULONG_PTR key, ULONG_PTR value, const IO_STATUS_BLOCK &iosb) {
    std::shared_ptr<Pending> p;
    const std::string *learnt = nullptr;
    {
        std::lock_guard<std::mutex> lk(g_hm);
        if (g_await.empty() && g_pending.empty()) return;
        auto a = g_await.find(value);
        if (a != g_await.end()) {
            auto h = g_h.find(a->second);
            if (h != g_h.end() && h->second.port == PORT_UNKNOWN) {
                h->second.port = PORT_BOUND;
                h->second.port_h = port;
                h->second.key = key;
                learnt = h->second.file;
            }
            g_await.erase(a);
        }
        auto it = g_pending.find(value);
        if (it != g_pending.end()) { p = it->second; g_pending.erase(it); }
    }
    if (p && iosb.Status >= 0) complete_pending(*p, iosb.Information);
    if (learnt) note_io(*learnt + ": its reads complete on a completion port (learnt from a dequeued packet)");
}

NTSTATUS NTAPI hk_remove(HANDLE port, PULONG_PTR key, PULONG_PTR value, PIO_STATUS_BLOCK iosb, PLARGE_INTEGER tmo) {
    NTSTATUS st = o_remove(port, key, value, iosb, tmo);
    if (st == 0 && key && value && iosb && *value) dequeued(port, *key, *value, *iosb);
    return st;
}

NTSTATUS NTAPI hk_remove_ex(HANDLE port, IoCompletionInfo *info, ULONG count, PULONG removed, PLARGE_INTEGER tmo,
                            BOOLEAN alertable) {
    NTSTATUS st = o_remove_ex(port, info, count, removed, tmo, alertable);
    if (st == 0 && info && removed)
        for (ULONG i = 0; i < *removed && i < count; ++i)
            if (info[i].value) dequeued(port, info[i].key, info[i].value, info[i].iosb);
    return st;
}

// ---- index records (grow mode) ------------------------------------------------------------------------------
struct Rec { uint64_t off = 0, id = 0; uint32_t size = 0; };
bool read_rec(uintptr_t a, Rec &r) {
    uint8_t b[20];
    if (g_api->read(a, b, sizeof b)) return false;
    memcpy(&r.off, b, 8); memcpy(&r.id, b + 8, 8); memcpy(&r.size, b + 16, 4);
    return true;
}
bool same(const Rec &a, const Rec &b) { return a.off == b.off && a.id == b.id && a.size == b.size; }

// Every 24-byte record {off, id, size} in the game's writable memory whose neighbours are `prev` and `next` (the
// entries before and after it in the archive), read-only. Seconds for a whole scan: the array base is kept.
std::vector<uintptr_t> scan_records(const Rec &r, const Rec *prev, const Rec *next) {
    std::vector<uintptr_t> hits;
    std::vector<uint8_t> chunk(4u << 20);
    SYSTEM_INFO si;
    GetSystemInfo(&si);
    uintptr_t a = (uintptr_t)si.lpMinimumApplicationAddress, max = (uintptr_t)si.lpMaximumApplicationAddress;
    while (a < max) {
        MEMORY_BASIC_INFORMATION m;
        if (!VirtualQuery((void *)a, &m, sizeof m)) break;
        uintptr_t base = (uintptr_t)m.BaseAddress, end = base + m.RegionSize;
        DWORD prot = m.Protect & 0xFF;
        if (m.State == MEM_COMMIT && m.Type != MEM_IMAGE && !(m.Protect & PAGE_GUARD) &&
            (prot == PAGE_READWRITE || prot == PAGE_WRITECOPY)) {
            for (uintptr_t p = base; p < end; p += chunk.size() - 32) {
                size_t n = (size_t)std::min<uintptr_t>(chunk.size(), end - p);
                if (n < 24 || g_api->read(p, chunk.data(), n)) break;
                for (size_t i = 0; i + 20 <= n; i += 8) {
                    uint64_t v;
                    memcpy(&v, &chunk[i], 8);
                    if (v != r.off) continue;
                    Rec x, nb;
                    if (!read_rec(p + i, x) || !same(x, r)) continue;
                    if (prev && (!read_rec(p + i - 24, nb) || !same(nb, *prev))) continue;
                    if (next && (!read_rec(p + i + 24, nb) || !same(nb, *next))) continue;
                    if (hits.empty() || hits.back() != p + i) hits.push_back(p + i);
                }
                if (n < chunk.size()) break;
            }
        }
        a = end > a ? end : a + si.dwPageSize;
    }
    return hits;
}

// ---- the manifest (live.py) and the loader thread -------------------------------------------------------------
struct Cell {                                          // one "cell" line
    std::string archive, map, name, mode, file;
    uint32_t index = 0;
    Rec rec, prev, next;
    bool has_prev = false, has_next = false;
    uint64_t data_size = 0;
};
struct Redirect {                                      // an entry whose record points into the virtual area
    std::vector<uintptr_t> addrs;
    Rec orig;
    uint64_t slot = 0, slot_size = 0;
    uint32_t size = 0;                                 // the size written
    bool active = false;
};

std::mutex g_sm;                                       // the status
Status g_status;
std::vector<std::pair<CellState, std::shared_ptr<std::atomic<uint64_t>>>> g_cells;
std::map<std::pair<std::string, uint32_t>, Redirect> g_redirects;   // loader thread only
std::map<std::string, uintptr_t> g_arrays;             // archive -> its record array (verified once)
std::map<std::string, uint64_t> g_vbase, g_vnext;      // archive -> its virtual area, next free slot
bool g_hooks_in = false;

bool parse_rec(const std::string &s, Rec &r) {          // "off,id,size" (decimal, hex id)
    unsigned long long a, b, c;
    if (sscanf(s.c_str(), "%llu,%llx,%llu", &a, &b, &c) != 3) return false;
    r.off = a; r.id = b; r.size = (uint32_t)c;
    return true;
}

std::vector<std::string> split(const std::string &line, char sep) {
    std::vector<std::string> out;
    size_t s = 0;
    for (;;) {
        size_t t = line.find(sep, s);
        out.push_back(line.substr(s, t == std::string::npos ? std::string::npos : t - s));
        if (t == std::string::npos) return out;
        s = t + 1;
    }
}

bool read_file(const std::wstring &path, std::string &out) {
    HANDLE f = CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, nullptr,
                           OPEN_EXISTING, 0, nullptr);
    if (f == INVALID_HANDLE_VALUE) return false;
    LARGE_INTEGER sz;
    bool ok = GetFileSizeEx(f, &sz) && sz.QuadPart < (1ll << 31);
    if (ok) {
        out.resize((size_t)sz.QuadPart);
        DWORD got = 0;
        size_t at = 0;
        while (ok && at < out.size()) {
            ok = ReadFile(f, &out[at], (DWORD)std::min<size_t>(out.size() - at, 1u << 30), &got, nullptr) && got;
            at += got;
        }
    }
    CloseHandle(f);
    return ok;
}

std::wstring widen(const std::string &s) { return std::wstring(s.begin(), s.end()); }

uint64_t vbase_of(const std::string &archive) {
    auto it = g_vbase.find(archive);
    if (it != g_vbase.end()) return it->second;
    WIN32_FILE_ATTRIBUTE_DATA fa;
    uint64_t size = 0;
    if (GetFileAttributesExW((g_game + widen(archive)).c_str(), GetFileExInfoStandard, &fa))
        size = (uint64_t)fa.nFileSizeHigh << 32 | fa.nFileSizeLow;
    uint64_t v = size ? virtual_base(size) : 0;
    g_vbase[archive] = v;
    g_vnext[archive] = v;
    return v;
}

// Write {off, size} into the record copies that still hold `expect` (the entry's own values or ours). The cell is
// loaded while it is edited (the player stands by it), so the game is not reading this record meanwhile.
bool write_rec(const std::vector<uintptr_t> &addrs, const Rec &expect, uint64_t off, uint32_t size) {
    bool ok = !addrs.empty();
    for (uintptr_t a : addrs) {
        Rec cur;
        if (!read_rec(a, cur) || !same(cur, expect)) { ok = false; continue; }
        if (g_api->write(a, &off, 8) || g_api->write(a + 16, &size, 4)) ok = false;
    }
    return ok;
}

// The entry's record(s), found once: at its place in the archive's array if that is known, else by a scan.
std::string find_records(const Cell &c, Redirect &rd) {
    if (!rd.addrs.empty()) return "";
    rd.orig = c.rec;
    auto arr = g_arrays.find(c.archive);
    Rec x;
    if (arr != g_arrays.end() && read_rec(arr->second + 24ull * c.index, x) && same(x, c.rec)) {
        rd.addrs.push_back(arr->second + 24ull * c.index);
        return "";
    }
    DWORD t0 = GetTickCount();
    rd.addrs = scan_records(c.rec, c.has_prev ? &c.prev : nullptr, c.has_next ? &c.next : nullptr);
    logf("live: %s %s: %d index record(s) found in %lu ms", c.archive.c_str(), c.name.c_str(), (int)rd.addrs.size(),
         (unsigned long)(GetTickCount() - t0));
    if (rd.addrs.empty()) return "its index record was not found in the game's memory";
    g_arrays[c.archive] = rd.addrs[0] - 24ull * c.index;
    return "";
}

// What the record holds now (ours or its own values); false if it is no longer this entry's.
bool current(const Redirect &rd, Rec &cur) {
    return !rd.addrs.empty() && read_rec(rd.addrs[0], cur) && cur.id == rd.orig.id;
}

void restore(const std::pair<std::string, uint32_t> &key, Redirect &rd) {
    Rec cur;
    if (rd.active && current(rd, cur)) write_rec(rd.addrs, cur, rd.orig.off, rd.orig.size);
    rd.active = false;
    logf("live: %s entry %u: index record restored", key.first.c_str(), key.second);
}

void install_hooks() {
    HMODULE nt = GetModuleHandleA("ntdll.dll");
    auto proc = [&](const char *n) { return (uintptr_t)GetProcAddress(nt, n); };
    g_query = (NtQueryInformationFile_t)proc("NtQueryInformationFile");
    g_setinfo = (NtSetInformationFile_t)proc("NtSetInformationFile");
    g_post = (NtSetIoCompletion_t)proc("NtSetIoCompletion");
    g_queue_apc = (NtQueueApcThread_t)proc("NtQueueApcThread");
    struct { const char *name; void *detour; void **orig; bool must; } hs[] = {
        {"NtCreateFile", (void *)hk_create, (void **)&o_create, false},
        {"NtOpenFile", (void *)hk_open, (void **)&o_open, false},
        {"NtSetInformationFile", (void *)hk_setinfo, (void **)&o_setinfo, false},
        {"NtRemoveIoCompletion", (void *)hk_remove, (void **)&o_remove, false},
        {"NtRemoveIoCompletionEx", (void *)hk_remove_ex, (void **)&o_remove_ex, false},
        {"NtReadFile", (void *)hk_read, (void **)&o_read, true},   // last: the others feed it
    };
    std::string failed;
    bool ok = g_query && g_setinfo && g_post && g_queue_apc;
    for (auto &h : hs) {
        uintptr_t at = proc(h.name);
        if (!at || g_api->hook(g_mod, at, h.detour, h.orig, nullptr)) {
            failed += std::string(failed.empty() ? "" : ", ") + h.name + " (" + g_api->last_error() + ")";
            if (h.must) ok = false;
        }
    }
    std::lock_guard<std::mutex> lk(g_sm);
    g_status.hooked = ok;
    if (!ok)
        g_status.note = "live edits off: could not hook " + failed + " (asset_trace hooks the same functions: do not "
                        "load both)";
    else if (!failed.empty())
        g_status.note = "hooked, except " + failed;
}

void load_manifest(const std::string &text) {
    std::vector<Cell> cells;
    int gen = -1, edits = 0;
    for (const std::string &line : split(text, '\n')) {
        if (line.empty() || line[0] == '#') continue;
        auto col = split(line, '\t');
        if (col[0] == "gen" && col.size() >= 4) {
            gen = atoi(col[1].c_str());
            edits = atoi(col[3].c_str());
        } else if (col[0] == "cell" && col.size() >= 11) {
            Cell c;
            c.archive = col[1];
            for (auto &ch : c.archive) ch = (char)tolower((unsigned char)ch);
            c.index = (uint32_t)strtoul(col[2].c_str(), nullptr, 10);
            if (!parse_rec(col[3], c.rec)) continue;
            c.has_prev = parse_rec(col[4], c.prev);
            c.has_next = parse_rec(col[5], c.next);
            c.mode = col[6];
            c.file = col[7];
            c.data_size = strtoull(col[8].c_str(), nullptr, 10);
            c.map = col[9];
            c.name = col[10];
            cells.push_back(c);
        }
    }
    if (gen < 0) return;                               // half a file? the next poll reads it again
    if (!g_hooks_in) { g_hooks_in = true; install_hooks(); }
    auto table = std::make_shared<Table>();
    std::vector<std::pair<CellState, std::shared_ptr<std::atomic<uint64_t>>>> states;
    std::set<std::pair<std::string, uint32_t>> grown;
    std::vector<std::pair<const Cell *, std::shared_ptr<std::vector<uint8_t>>>> later;   // grow: after publishing
    for (const Cell &c : cells) {
        CellState cs;
        cs.map = c.map; cs.name = c.name; cs.mode = c.mode;
        auto counter = std::make_shared<std::atomic<uint64_t>>(0);
        std::string bytes;
        if (c.file.find_first_of("\\/:") != std::string::npos || !read_file(g_dir + L"live\\" + widen(c.file), bytes) ||
            bytes.size() != c.data_size) {
            cs.state = "its data file is missing or incomplete";
            states.push_back({cs, counter});
            continue;
        }
        auto data = std::make_shared<std::vector<uint8_t>>(bytes.begin(), bytes.end());
        if (c.mode == "same" && data->size() == c.rec.size) {
            FileTable &ft = table->files[c.archive];
            ft.ranges.push_back(Range{c.rec.off, data->data(), data->size()});
            ft.reads.push_back(counter);
            ft.owners.push_back(data);
            cs.state = "waiting for the game to read it (ride away and back)";
        } else if (c.mode == "grow") {
            grown.insert({c.archive, c.index});
            later.push_back({&c, data});
            cs.state = "";
        } else {
            cs.state = "bad mode / size";
        }
        states.push_back({cs, counter});
    }
    // grow: each entry's record is pointed at a slot in the archive's virtual area; the table that serves the slot
    // is published before the record is written, so the game cannot read a slot that is not served yet
    struct Write { std::pair<std::string, uint32_t> key; const Cell *c; size_t state; uint64_t slot; uint32_t size; };
    std::vector<Write> writes;
    for (auto &[c, data] : later) {
        size_t si = 0;
        while (si < states.size() && !(states[si].first.name == c->name && states[si].first.map == c->map)) ++si;
        std::pair<std::string, uint32_t> key{c->archive, c->index};
        Redirect &r = g_redirects[key];
        std::string why = find_records(*c, r);
        uint64_t vb = why.empty() ? vbase_of(c->archive) : 0;
        if (why.empty() && !vb) why = "the archive's size is unknown";
        if (why.empty() && data->size() >= (1ull << 32)) why = "too big";
        if (!why.empty()) {
            states[si].first.state = "not redirected (shows at the next game start): " + why;
            logf("live: %s %s: %s", c->archive.c_str(), c->name.c_str(), why.c_str());
            continue;
        }
        if (!r.slot || data->size() > r.slot_size) {   // a slot of its own, kept while the data fits
            r.slot = g_vnext[c->archive];
            r.slot_size = slot_size(data->size());
            g_vnext[c->archive] += r.slot_size;
        }
        FileTable &ft = table->files[c->archive];
        ft.vbase = vb;
        ft.ranges.push_back(Range{r.slot, data->data(), data->size()});
        ft.reads.push_back(states[si].second);
        ft.owners.push_back(data);
        states[si].first.state = "waiting for the game to read it (ride away and back)";
        writes.push_back({key, c, si, r.slot, (uint32_t)data->size()});
    }
    // what was served in the virtual area before stays served for one more generation (a record about to be
    // restored, a slot the record pointed at a moment ago)
    auto old = snapshot();
    for (auto &[name, oft] : old->files) {
        if (oft.vbase == UINT64_MAX) continue;
        FileTable &ft = table->files[name];
        for (size_t i = 0; i < oft.ranges.size(); ++i) {
            if (oft.ranges[i].off < oft.vbase || oft.carried[i]) continue;
            bool clash = false;
            for (auto &r : ft.ranges) clash |= overlaps(r, oft.ranges[i].off, oft.ranges[i].size);
            if (clash) continue;
            ft.ranges.push_back(oft.ranges[i]);
            ft.reads.push_back(oft.reads[i]);
            ft.owners.push_back(oft.owners[i]);
            ft.carried.resize(ft.ranges.size() - 1);
            ft.carried.push_back(true);
        }
        if (ft.vbase == UINT64_MAX) ft.vbase = oft.vbase;
    }
    for (auto &[name, ft] : table->files) ft.carried.resize(ft.ranges.size());
    std::atomic_store(&g_table, std::shared_ptr<const Table>(table));
    g_any.store(true, std::memory_order_release);
    for (Write &w : writes) {
        Redirect &r = g_redirects[w.key];
        Rec cur;
        std::string why;
        if (!current(r, cur)) why = "its index record moved";
        else if (!write_rec(r.addrs, cur, w.slot, w.size)) why = "its index record could not be written";
        if (why.empty()) {
            r.active = true;
            r.size = w.size;
        } else {
            states[w.state].first.state = "not redirected (shows at the next game start): " + why;
            logf("live: %s %s: %s", w.c->archive.c_str(), w.c->name.c_str(), why.c_str());
        }
    }
    for (auto &[key, rd] : g_redirects)
        if (rd.active && !grown.count(key)) restore(key, rd);
    {
        std::lock_guard<std::mutex> lk(g_sm);
        g_status.helper = true;
        g_status.gen = gen;
        g_status.edits = edits;
        g_cells = states;
    }
    logf("live: generation %d (edits up to #%d): %d cell(s) served", gen, edits, (int)cells.size());
}

DWORD WINAPI loader(LPVOID) {
    t_inside = true;                                   // this thread's reads are ours
    std::wstring path = g_dir + L"live\\manifest.tsv";
    FILETIME seen = {0, 0};
    uint64_t seen_size = UINT64_MAX;
    for (;;) {
        WIN32_FILE_ATTRIBUTE_DATA fa;
        if (GetFileAttributesExW(path.c_str(), GetFileExInfoStandard, &fa)) {
            uint64_t size = (uint64_t)fa.nFileSizeHigh << 32 | fa.nFileSizeLow;
            if (CompareFileTime(&fa.ftLastWriteTime, &seen) != 0 || size != seen_size) {
                std::string text;
                if (read_file(path, text) && text.size() == size) {
                    seen = fa.ftLastWriteTime;
                    seen_size = size;
                    load_manifest(text);
                }
            }
        } else {
            std::lock_guard<std::mutex> lk(g_sm);
            if (!g_status.helper && g_status.note.empty())
                g_status.note = "live edits off: no session helper (mods/world_editor/live.py; WORLD_EDITOR_LIVE=0?)";
        }
        Sleep(1000);
    }
}

}  // namespace

void init(const PowderApi *api, PowderMod *mod, const std::wstring &dir, const std::wstring &game_dir) {
    g_api = api;
    g_mod = mod;
    g_dir = dir;
    g_game = game_dir;
    CreateThread(nullptr, 0, loader, nullptr, 0, nullptr);
}

void poke() {
    if (g_dir.empty()) return;
    HANDLE f = CreateFileW((g_dir + L"live\\poke").c_str(), GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_DELETE, nullptr,
                           CREATE_ALWAYS, 0, nullptr);
    if (f == INVALID_HANDLE_VALUE) return;
    char b[24];
    DWORD n = (DWORD)snprintf(b, sizeof b, "%lu\n", (unsigned long)GetTickCount()), w;
    WriteFile(f, b, n, &w, nullptr);
    CloseHandle(f);
}

Status status() {
    Status s;
    {
        std::lock_guard<std::mutex> lk(g_sm);
        s = g_status;
        for (auto &[cs, counter] : g_cells) {
            CellState c = cs;
            c.reads = counter->load();
            if (c.reads) c.state = "read " + std::to_string(c.reads) + " time(s): shown when the cell streamed in";
            s.cells.push_back(c);
        }
    }
    std::lock_guard<std::mutex> lk(g_hm);
    for (auto &n : g_io_notes) s.io += (s.io.empty() ? "" : "\n") + n;
    return s;
}

}  // namespace live
