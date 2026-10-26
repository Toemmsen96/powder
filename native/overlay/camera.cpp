#include "camera.h"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <atomic>
#include <cmath>
#include <cstring>
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

void calibrate(uintptr_t player_pos) {
    P3 me0;
    if (!rpm(player_pos, &me0, 12)) { set_status("no player position"); g_busy = false; return; }
    set_status("scanning around the rider...");
    std::vector<uintptr_t> cand = snapshot(me0);
    std::vector<P3> first(cand.size()), last(cand.size());
    for (size_t i = 0; i < cand.size(); ++i) if (!rpm(cand[i], &first[i], 12)) first[i] = {NAN, NAN, NAN};
    std::vector<int> moved(cand.size(), 0);
    last = first;
    set_status("now turn the camera (rider standing still)");
    for (int round = 0; round < 120 && g_busy; ++round) {      // up to ~30 s
        Sleep(250);
        P3 me;
        if (!rpm(player_pos, &me, 12)) break;
        if (dist(me, me0) > 0.05f) { me0 = me; last.clear(); break; }   // the rider moved: start over
        std::map<std::tuple<int, int, int>, std::vector<size_t>> groups;
        for (size_t i = 0; i < cand.size(); ++i) {
            P3 v;
            if (!rpm(cand[i], &v, 12) || std::isnan(first[i].x)) continue;
            float d0 = dist(first[i], me), d1 = dist(v, me);
            if (std::fabs(d1 - d0) > 0.3f || d1 < 1.5f || d1 > 15) { first[i].x = NAN; continue; }   // not an orbit
            if (dist(v, last[i]) > 0.3f) moved[i]++;
            last[i] = v;
            if (moved[i] >= 2) groups[{(int)std::lround(v.x * 10), (int)std::lround(v.y * 10), (int)std::lround(v.z * 10)}].push_back(i);
        }
        size_t best = 0; const std::vector<size_t> *bg = nullptr;
        for (auto &kv : groups) if (kv.second.size() > best) { best = kv.second.size(); bg = &kv.second; }
        if (bg && best >= 2) {                                       // copies agree: the camera
            g_cam = cand[(*bg)[0]];
            set_status("found (" + std::to_string(best) + " copies agree)");
            g_busy = false;
            return;
        }
    }
    if (!g_cam) set_status(last.empty() ? "the rider moved - stand still and try again"
                                        : "not found - turn the camera while standing still, then try again");
    g_busy = false;
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
    if (cam && player_pos && now - g_checked > 1000) {              // still near the rider?
        g_checked = now;
        P3 c, me;
        if (!rpm(cam, &c, 12) || !rpm(player_pos, &me, 12) || dist(c, me) < 1.0f || dist(c, me) > 25.0f) {
            g_cam = 0;
            set_status("lost (map change?) - looking again");
            camera_calibrate(player_pos);
            return 0;
        }
    }
    return cam;
}

const char *camera_status() {
    static thread_local std::string copy;
    std::lock_guard<std::mutex> lk(g_m);
    copy = g_status;
    return copy.c_str();
}

} // namespace pw
