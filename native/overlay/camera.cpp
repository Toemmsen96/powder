#include "camera.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cmath>
#include <cstring>
#include <algorithm>
#include <map>
#include <mutex>
#include <thread>
#include <vector>

namespace pw {

namespace {

struct P3 { float x, y, z; };

std::mutex g_m;
std::atomic<bool> g_busy{false};
std::atomic<uintptr_t> g_cam{0};
std::string g_status = "not looked for yet";

// Every copy the calibration found (most are per-frame scratch that the game may stop updating or reuse), and
// for each: its last value, and where the rider was when it last changed. Guarded by g_m.
struct Copy { uintptr_t addr; P3 last; P3 me_at_change; };
std::vector<Copy> g_copies;
DWORD g_checked = 0;

bool rpm(uintptr_t a, void *out, size_t n) {
    SIZE_T got = 0;
    return ReadProcessMemory(GetCurrentProcess(), (void *)a, out, n, &got) && got == n;
}

float dist(const P3 &a, const P3 &b) {
    float dx = a.x - b.x, dy = a.y - b.y, dz = a.z - b.z;
    return std::sqrt(dx * dx + dy * dy + dz * dz);
}

void set_status(const std::string &s) { std::lock_guard<std::mutex> lk(g_m); g_status = s; }

// Every float3 1.5-15 m from `me` in private read-write memory (4-byte aligned).
std::vector<uintptr_t> snapshot(const P3 &me) {
    std::vector<uintptr_t> out;
    std::vector<uint8_t> buf(4 << 20);
    MEMORY_BASIC_INFORMATION m;
    for (uintptr_t a = 0x10000; a < 0x7FFFFFFF0000ull && VirtualQuery((void *)a, &m, sizeof(m)); ) {
        uintptr_t end = (uintptr_t)m.BaseAddress + m.RegionSize;
        bool rw = m.State == MEM_COMMIT && m.Type == MEM_PRIVATE && !(m.Protect & PAGE_GUARD) &&
                  ((m.Protect & 0xFF) == PAGE_READWRITE);
        for (uintptr_t p = (uintptr_t)m.BaseAddress; rw && p < end; p += buf.size() - 8) {
            size_t n = std::min<uintptr_t>(buf.size(), end - p);
            if (n < 12 || !rpm(p, buf.data(), n)) break;
            const float *f = (const float *)buf.data();
            for (size_t i = 0; i + 3 <= n / 4; ++i) {
                float dx = f[i] - me.x, dy = f[i + 1] - me.y, dz = f[i + 2] - me.z;
                if (std::fabs(dx) > 15 || std::fabs(dy) > 15 || std::fabs(dz) > 15) continue;
                float d2 = dx * dx + dy * dy + dz * dz;
                if (d2 > 2.25f && d2 < 225.0f) out.push_back(p + 4 * i);
            }
            if (out.size() > 2000000) return out;              // a cap, should a scan go wild
        }
        a = end;
    }
    return out;
}

// One attempt: snapshot around the rider, then watch for points that orbit it while it stands still. Returns
// the agreeing copies (empty: none yet), sets `moved` if the rider moved (the snapshot is then stale).
std::vector<uintptr_t> attempt(uintptr_t player_pos, bool &moved) {
    moved = false;
    P3 me0;
    if (!rpm(player_pos, &me0, 12)) return {};
    set_status("scanning around the rider...");
    std::vector<uintptr_t> cand = snapshot(me0);
    std::vector<P3> first(cand.size()), last(cand.size());
    std::vector<float> d0(cand.size());
    for (size_t i = 0; i < cand.size(); ++i) {
        if (!rpm(cand[i], &first[i], 12)) first[i] = {NAN, NAN, NAN};
        d0[i] = dist(first[i], me0);
    }
    std::vector<int> moves(cand.size(), 0);
    last = first;
    set_status("now turn the camera (rider standing still)");
    for (int round = 0; round < 80 && g_busy; ++round) {       // ~20 s per snapshot
        Sleep(250);
        P3 me;
        if (!rpm(player_pos, &me, 12)) return {};
        if (dist(me, me0) > 0.05f) { moved = true; return {}; }
        std::map<std::tuple<int, int, int>, std::vector<size_t>> groups;
        for (size_t i = 0; i < cand.size(); ++i) {
            P3 v;
            if (std::isnan(first[i].x) || !rpm(cand[i], &v, 12)) continue;
            float d1 = dist(v, me);
            // not an orbit (the camera's distance springs a little when it turns, or it is pushed in by terrain)
            if (std::fabs(d1 - d0[i]) > 0.75f || d1 < 1.0f || d1 > 15) { first[i].x = NAN; continue; }
            if (dist(v, last[i]) > 0.15f) moves[i]++;
            last[i] = v;
            if (moves[i] >= 2) groups[{(int)std::lround(v.x * 10), (int)std::lround(v.y * 10), (int)std::lround(v.z * 10)}].push_back(i);
        }
        const std::vector<size_t> *bg = nullptr;
        for (auto &kv : groups) if (!bg || kv.second.size() > bg->size()) bg = &kv.second;
        if (bg && bg->size() >= 2) {                                 // copies agree: the camera
            std::vector<size_t> idx = *bg;                           // the ones that moved most first
            std::stable_sort(idx.begin(), idx.end(), [&](size_t a, size_t b) { return moves[a] > moves[b]; });
            std::vector<uintptr_t> out;
            for (size_t i : idx) { out.push_back(cand[i]); if (out.size() == 32) break; }
            return out;
        }
    }
    return {};
}

void calibrate(uintptr_t player_pos) {
    // Up to ~2 minutes. When the rider moves, wait until it stands still and take a new snapshot, rather than
    // giving up and needing another "Find camera".
    DWORD t0 = GetTickCount();
    while (g_busy && GetTickCount() - t0 < 120000) {
        bool moved = false;
        std::vector<uintptr_t> found = attempt(player_pos, moved);
        if (!found.empty()) {
            P3 me{};
            rpm(player_pos, &me, 12);
            {
                std::lock_guard<std::mutex> lk(g_m);
                g_copies.clear();
                for (uintptr_t a : found) { Copy c{a, {}, me}; rpm(a, &c.last, 12); g_copies.push_back(c); }
                g_status = "found (" + std::to_string(found.size()) + " copies agree)";
            }
            g_cam = found[0];
            g_busy = false;
            return;
        }
        if (!g_busy) break;
        if (moved) {
            set_status("waiting for the rider to stand still...");
            P3 a{}, b{};
            do {                                                     // still for half a second
                if (!rpm(player_pos, &a, 12)) { g_busy = false; set_status("no player position"); return; }
                Sleep(500);
                if (!rpm(player_pos, &b, 12)) { g_busy = false; set_status("no player position"); return; }
            } while (g_busy && dist(a, b) > 0.05f && GetTickCount() - t0 < 120000);
        }
    }
    if (!g_cam) set_status("not found - turn the camera while standing still, then try again");
    g_busy = false;
}

// A copy still follows the camera: near the rider (it trails behind when flying fast), and not frozen while
// the rider went on without it.
bool copy_ok(Copy &c, const P3 &me) {
    P3 v;
    if (!rpm(c.addr, &v, 12)) return false;
    float d = dist(v, me);
    if (d < 0.5f || d > 60.0f) return false;
    if (dist(v, c.last) > 0.01f) { c.last = v; c.me_at_change = me; return true; }
    return dist(me, c.me_at_change) < 3.0f;
}

} // namespace

void camera_calibrate(uintptr_t player_pos) {
    if (!player_pos || g_busy.exchange(true)) return;
    g_cam = 0;
    std::thread(calibrate, player_pos).detach();
}

uintptr_t camera_pos(uintptr_t player_pos) {
    uintptr_t cam = g_cam;
    DWORD now = GetTickCount();
    if (!cam || !player_pos || now - g_checked < 250) return cam;
    g_checked = now;                                                 // still following? else another copy
    P3 me;
    if (!rpm(player_pos, &me, 12)) return cam;
    {
        std::lock_guard<std::mutex> lk(g_m);
        uintptr_t next = 0;
        for (Copy &c : g_copies)                                     // update them all, keep the first good one
            if (copy_ok(c, me) && !next) next = c.addr;
        if (next) { g_cam = next; return next; }
        g_copies.clear();
        g_status = "lost (map change?) - looking again";
    }
    g_cam = 0;
    camera_calibrate(player_pos);
    return 0;
}

const char *camera_status() {
    static thread_local std::string copy;
    std::lock_guard<std::mutex> lk(g_m);
    copy = g_status;
    return copy.c_str();
}

} // namespace pw
