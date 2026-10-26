#include "noclip.h"
#include "memscan.h"
#include "codepatch.h"
#include "crashlog.h"
#include "json.hpp"
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <map>
#include <vector>

extern "C" {
#include "hde64.h"
}

namespace pw {

namespace {

struct Chain { uint64_t root; std::vector<int64_t> offsets; };
struct Writer { uint64_t rva; std::vector<uint8_t> bytes; };

std::string g_module;
uint64_t g_pos_offset = 0;
std::vector<Chain> g_chains;
std::vector<Writer> g_writers;
bool g_loaded = false;

// The cave block: [lo][hi] then one cave per writer. lo/hi are read by every cave (rip-relative).
uint8_t *g_block = nullptr;
volatile uint64_t *g_lo = nullptr, *g_hi = nullptr;
bool g_installed = false;

uint64_t parse_hex(const nlohmann::json &v) {
    if (v.is_number_unsigned() || v.is_number_integer()) return v.get<uint64_t>();
    return std::stoull(v.get<std::string>(), nullptr, 16);
}

std::vector<uint8_t> parse_bytes(const std::string &hex) {
    std::vector<uint8_t> out;
    for (size_t i = 0; i + 1 < hex.size(); i += 2) out.push_back((uint8_t)std::stoul(hex.substr(i, 2), nullptr, 16));
    return out;
}

uintptr_t module_base() {
    return (uintptr_t)(g_module.empty() ? GetModuleHandleW(nullptr) : GetModuleHandleA(g_module.c_str()));
}

bool read64(uintptr_t a, uint64_t &out) {
    Value v;
    if (!peek(a, ValType::I64, v)) return false;
    out = (uint64_t)v.i64;
    return true;
}

void put32(std::vector<uint8_t> &c, size_t at, int32_t v) { memcpy(&c[at], &v, 4); }

// Build the cave for one writer at `site` (whose first `covered` bytes are relocated), placed at `cave`.
bool build_cave(uintptr_t site, uintptr_t cave, std::vector<uint8_t> &code, size_t &covered, std::string &err) {
    std::vector<hde64s> ins;
    covered = 0;
    while (covered < 5) {
        hde64s hs;
        hde64_disasm((const void *)(site + covered), &hs);
        if (hs.flags & F_ERROR) { err = "undecodable instruction"; return false; }
        bool riprel = (hs.flags & F_MODRM) && hs.modrm_mod == 0 && hs.modrm_rm == 5;
        if (riprel || (hs.flags & F_RELATIVE) || hs.opcode == 0xC3 || hs.opcode == 0xC2) {
            err = "a rip-relative / branch / ret instruction in the patched bytes"; return false;
        }
        ins.push_back(hs);
        covered += hs.len;
    }
    const hde64s &w = ins[0];
    if (!(w.flags & F_MODRM) || w.modrm_mod == 3) { err = "the writer has no memory operand"; return false; }
    bool has_sib = (w.flags & F_SIB) != 0;
    int base = has_sib ? (w.sib_base | (w.rex_b << 3)) : (w.modrm_rm | (w.rex_b << 3));
    if (base == 4) { err = "stack-relative writer"; return false; }

    std::vector<uint8_t> c;
    c.push_back(0x9C);                                          // pushfq
    c.push_back(0x50);                                          // push rax
    c.push_back(0x48 | (w.rex_x << 1) | w.rex_b);               // lea rax, [same operand as the writer]
    c.push_back(0x8D);
    c.push_back((uint8_t)((w.modrm_mod << 6) | w.modrm_rm));
    if (has_sib) c.push_back(w.sib);
    if (w.flags & F_DISP8) c.push_back(w.disp.disp8);
    else if (w.flags & F_DISP32) { int32_t d = (int32_t)w.disp.disp32; c.insert(c.end(), (uint8_t *)&d, (uint8_t *)&d + 4); }
    auto cmp_rip = [&](uintptr_t target) {                      // cmp rax, [rip+target]
        c.insert(c.end(), {0x48, 0x3B, 0x05, 0, 0, 0, 0});
        put32(c, c.size() - 4, (int32_t)(target - (cave + c.size())));
    };
    cmp_rip((uintptr_t)g_lo);
    c.insert(c.end(), {0x72, 0}); size_t jb = c.size() - 1;     // jb  write
    cmp_rip((uintptr_t)g_hi);
    c.insert(c.end(), {0x73, 0}); size_t jae = c.size() - 1;    // jae write
    c.insert(c.end(), {0x58, 0x9D});                            // pop rax; popfq
    c.insert(c.end(), {0xEB, 0}); size_t jskip = c.size() - 1;  // jmp past the writer
    size_t write = c.size();
    c.insert(c.end(), {0x58, 0x9D});                            // write: pop rax; popfq
    c.insert(c.end(), (const uint8_t *)site, (const uint8_t *)site + w.len);   // the writer
    size_t after = c.size();
    c.insert(c.end(), (const uint8_t *)site + w.len, (const uint8_t *)site + covered);   // the rest, as is
    c.insert(c.end(), {0xFF, 0x25, 0, 0, 0, 0});                // jmp [rip+0] -> back
    uint64_t back = site + covered;
    c.insert(c.end(), (uint8_t *)&back, (uint8_t *)&back + 8);
    c[jb] = (uint8_t)(write - (jb + 1));
    c[jae] = (uint8_t)(write - (jae + 1));
    c[jskip] = (uint8_t)(after - (jskip + 1));
    code = c;
    return true;
}

} // namespace

bool trainer_load(const wchar_t *path, std::string &err) {
    FILE *f = _wfopen(path, L"rb");
    if (!f) { err = "no powder_trainer.json beside the overlay"; return false; }
    std::string text;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof(buf), f)) > 0) text.append(buf, n);
    fclose(f);
    try {
        auto j = nlohmann::json::parse(text);
        g_module = j.value("module", "");
        g_pos_offset = parse_hex(j["position"]["offset"]);
        g_chains.clear();
        for (auto &c : j["position"]["chains"]) {
            Chain ch{parse_hex(c["root"]), {}};
            for (auto &o : c["offsets"]) ch.offsets.push_back((int64_t)parse_hex(o));
            g_chains.push_back(ch);
        }
        g_writers.clear();
        for (auto &w : j["writers"]) g_writers.push_back({parse_hex(w["rva"]), parse_bytes(w["bytes"].get<std::string>())});
    } catch (const std::exception &e) {
        err = std::string("powder_trainer.json: ") + e.what();
        return false;
    }
    g_loaded = true;
    return true;
}

bool trainer_loaded() { return g_loaded; }


uintptr_t find_player_pos(std::string &why) {
    uintptr_t base = module_base();
    if (!g_loaded || !base) { why = "no trainer data"; return 0; }
    std::map<uintptr_t, int> votes;
    for (auto &ch : g_chains) {
        uint64_t a = base + ch.root;
        bool ok = true;
        for (int64_t off : ch.offsets) {
            uint64_t v;
            if (!read64(a, v) || v < 0x10000) { ok = false; break; }
            a = v + off;
        }
        if (!ok) continue;
        uintptr_t pos = a + g_pos_offset;
        Value x, y, z;
        if (!peek(pos, ValType::F32, x) || !peek(pos + 4, ValType::F32, y) || !peek(pos + 8, ValType::F32, z)) continue;
        float v[3] = {x.f32, y.f32, z.f32};
        bool sane = true, big = false;
        for (float t : v) { sane &= std::isfinite(t) && std::fabs(t) < 1e6f; big |= std::fabs(t) > 1.0f; }
        if (sane && big) votes[pos]++;
    }
    if (votes.empty()) { why = "no chain resolves (new game build? re-run tools/trainer)"; return 0; }
    auto best = votes.begin();
    for (auto it = votes.begin(); it != votes.end(); ++it) if (it->second > best->second) best = it;
    char msg[96];
    if (best->second < 2 && g_chains.size() > 2) {      // one chain alone may be a coincidence: never write there
        snprintf(msg, sizeof(msg), "chains disagree (%zu candidates, none twice)", votes.size());
        why = msg;
        return 0;
    }
    snprintf(msg, sizeof(msg), "%d of %zu chains agree", best->second, g_chains.size());
    why = msg;
    return best->first;
}

bool noclip_install(std::string &err) {
    if (g_installed) return true;
    if (!g_loaded || g_writers.empty()) { err = "no writers in powder_trainer.json"; return false; }
    uintptr_t base = module_base();
    if (!base) { err = "module not loaded"; return false; }
    if (!g_block) {
        g_block = alloc_near(base, 0x1000);
        if (!g_block) { err = "no free memory near the module for the caves"; return false; }
        crashlog_add_range((uintptr_t)g_block, (uintptr_t)g_block + 0x1000);
        g_lo = (volatile uint64_t *)g_block;
        g_hi = (volatile uint64_t *)(g_block + 8);
        *g_lo = 0; *g_hi = 0;
    }
    // one 128-byte cave slot per writer; a retry after a partial install skips the sites already ours
    for (size_t i = 0; i < g_writers.size(); i++) {
        const Writer &w = g_writers[i];
        uintptr_t site = base + w.rva, cave = (uintptr_t)g_block + 64 + 128 * i;
        int32_t r = (int32_t)((int64_t)cave - (int64_t)(site + 5));
        if (((const uint8_t *)site)[0] == 0xE9 && memcmp((const uint8_t *)site + 1, &r, 4) == 0) continue;
        if (memcmp((void *)site, w.bytes.data(), w.bytes.size()) != 0) {
            err = ((const uint8_t *)site)[0] == 0xE9 ? "a writer is hooked already (tools/trainer/skiphook.py? remove it)"
                                                      : "a writer's bytes differ (another game build?)";
            return false;
        }
        if (64 + 128 * (i + 1) > 0x1000) { err = "cave block full"; return false; }
        std::vector<uint8_t> code;
        size_t covered;
        if (!build_cave(site, cave, code, covered, err)) return false;
        if (code.size() > 128) { err = "cave too large"; return false; }
        int64_t rel = (int64_t)cave - (int64_t)(site + 5);
        if (rel < INT32_MIN || rel > INT32_MAX) { err = "cave out of jmp range"; return false; }
        memcpy((void *)cave, code.data(), code.size());
        std::vector<uint8_t> jmp(covered, 0x90);
        jmp[0] = 0xE9;
        memcpy(&jmp[1], &r, 4);
        std::string how;
        if (!write_code(site, jmp.data(), jmp.size(), how)) { err = "could not patch the game's code (" + how + ")"; return false; }
    }
    g_installed = true;
    return true;
}

bool noclip_installed() { return g_installed; }

void noclip_set_range(uintptr_t lo, uintptr_t hi) {
    if (!g_installed) return;
    if (hi > lo) { *g_lo = lo; *g_hi = hi; }    // a cave reading (old lo, new hi) still only skips the player
    else { *g_hi = 0; *g_lo = 0; }
}

bool noclip_active() { return g_installed && *g_hi > *g_lo; }

} // namespace pw
