// Native test of live_patch.h (the byte-range logic of src/live.cpp), run by test_live.py:
//
//     g++ -std=c++17 -O1 -I../src live_patch_test.cpp -o live_patch_test
//     live_patch_test                                  # the unit cases only
//     live_patch_test <game dir> <live dir> <out dir>  # + every cell of <live dir>/manifest.tsv read back
//
// The second form plays the game: each served cell's entry is read from its archive in aligned chunks of the
// sizes Steep uses (4 KB .. 128 KB, from aligned starts below the entry), the served bytes are overlaid as the
// hook does, and the entry's bytes are written to <out dir>/<id>.bin for test_live.py to parse. "grow" cells are
// read from their virtual slot (as the redirected index record makes the game do). Exit status 0 = all good.
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <map>
#include <random>
#include <sstream>
#include <string>
#include <vector>
#include "live_patch.h"

using namespace live;

static int fails = 0;
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); ++fails; } } while (0)

static void unit() {
    std::vector<uint8_t> a(100, 0xAA), b(50, 0xBB);
    std::vector<Range> rs = {{1000, a.data(), a.size()}, {1100, b.data(), b.size()}};   // adjacent
    CHECK(!any_overlap(rs, 0, 1000));
    CHECK(any_overlap(rs, 0, 1001));
    CHECK(!any_overlap(rs, 1150, 4096));
    CHECK(any_overlap(rs, 1149, 1));
    CHECK(!any_overlap(rs, 1000, 0));
    std::vector<uint8_t> buf(4096, 0x11);
    CHECK(overlay(rs, 0, buf.data(), buf.size()) == 150);
    CHECK(buf[999] == 0x11 && buf[1000] == 0xAA && buf[1099] == 0xAA && buf[1100] == 0xBB && buf[1149] == 0xBB &&
          buf[1150] == 0x11);
    std::vector<uint8_t> small(10, 0x11);                       // a read inside one patch
    CHECK(overlay(rs, 1095, small.data(), small.size()) == 10);
    CHECK(small[4] == 0xAA && small[5] == 0xBB);
    std::vector<uint8_t> v(4096, 0x22);                         // virtual: zeros + patches
    std::vector<Range> vr = {{virtual_base(5000), a.data(), a.size()}};
    CHECK(virtual_read(vr, virtual_base(5000) - 10, v.data(), 64) == 64);
    CHECK(v[9] == 0 && v[10] == 0xAA && v[63] == 0xAA && v[64] == 0x22);
    CHECK(virtual_base(1) == 257 * MIB && virtual_base(MIB) == 257 * MIB && virtual_base(MIB + 1) == 258 * MIB);
    CHECK(slot_size(1) == MIB && slot_size(MIB) == 2 * MIB);
}

struct Cell { std::string archive, mode, file, id; uint64_t off = 0, size = 0, bytes = 0; };

static std::vector<std::string> split(const std::string &s, char c) {
    std::vector<std::string> out;
    std::stringstream ss(s);
    std::string x;
    while (std::getline(ss, x, c)) out.push_back(x);
    return out;
}

static bool slurp(const std::string &path, std::vector<uint8_t> &out) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return false;
    out.assign(std::istreambuf_iterator<char>(f), {});
    return true;
}

int main(int argc, char **argv) {
    unit();
    if (argc < 4) {
        printf("unit cases: %s\n", fails ? "FAILED" : "ok");
        return fails ? 1 : 0;
    }
    std::string game = argv[1], dir = argv[2], out = argv[3];
    std::ifstream mf(dir + "/manifest.tsv");
    std::string line;
    std::vector<Cell> cells;
    while (std::getline(mf, line)) {
        auto c = split(line, '\t');
        if (c.size() < 11 || c[0] != "cell") continue;
        Cell x;
        x.archive = c[1];
        auto rec = split(c[3], ',');
        x.off = strtoull(rec[0].c_str(), nullptr, 10);
        x.id = rec[1];
        x.size = strtoull(rec[2].c_str(), nullptr, 10);
        x.mode = c[6];
        x.file = c[7];
        x.bytes = strtoull(c[8].c_str(), nullptr, 10);
        cells.push_back(x);
    }
    // the served table, as live.cpp builds it: same-size at the entry, grow at a slot per entry
    std::map<std::string, std::vector<Range>> table;
    std::map<std::string, uint64_t> vnext;
    std::vector<std::vector<uint8_t>> datas(cells.size());
    std::vector<uint64_t> where(cells.size());
    for (size_t i = 0; i < cells.size(); ++i) {
        Cell &c = cells[i];
        CHECK(slurp(dir + "/" + c.file, datas[i]) && datas[i].size() == c.bytes);
        if (c.mode == "same") {
            CHECK(c.bytes == c.size);
            where[i] = c.off;
        } else {
            std::ifstream f(game + "/" + c.archive, std::ios::binary | std::ios::ate);
            uint64_t fsize = (uint64_t)f.tellg();
            if (!vnext.count(c.archive)) vnext[c.archive] = virtual_base(fsize);
            where[i] = vnext[c.archive];
            vnext[c.archive] += slot_size(c.bytes);
            CHECK(where[i] >= fsize + 256 * MIB);
        }
        table[c.archive].push_back(Range{where[i], datas[i].data(), datas[i].size()});
    }
    std::mt19937 rng(1234);
    const size_t sizes[] = {4096, 8192, 24576, 106496, 131072, 655360};
    size_t reads = 0, patched = 0;
    for (size_t i = 0; i < cells.size(); ++i) {
        Cell &c = cells[i];
        uint64_t want = c.mode == "same" ? c.size : c.bytes, start = where[i];
        std::vector<uint8_t> entry(want);
        std::FILE *f = std::fopen((game + "/" + c.archive).c_str(), "rb");
        CHECK(f);
        if (!f) continue;
        std::ifstream sz(game + "/" + c.archive, std::ios::binary | std::ios::ate);
        uint64_t vbase = virtual_base((uint64_t)sz.tellg());
        uint64_t pos = start & ~4095ull;                           // aligned reads covering the entry
        while (pos < start + want) {
            size_t n = sizes[rng() % 6];
            std::vector<uint8_t> buf(n, 0xEE);
            size_t got;
            if (pos >= vbase) {
                got = virtual_read(table[c.archive], pos, buf.data(), n);
            } else {
                std::fseek(f, (long)pos, SEEK_SET);
                got = std::fread(buf.data(), 1, n, f);
                if (any_overlap(table[c.archive], pos, got)) {
                    overlay(table[c.archive], pos, buf.data(), got);
                    ++patched;
                }
            }
            ++reads;
            for (uint64_t k = 0; k < got; ++k)
                if (pos + k >= start && pos + k < start + want) entry[pos + k - start] = buf[k];
            pos += n;
        }
        std::fclose(f);
        std::FILE *o = std::fopen((out + "/" + c.id + ".bin").c_str(), "wb");
        CHECK(o && std::fwrite(entry.data(), 1, entry.size(), o) == entry.size());
        if (o) std::fclose(o);
    }
    // reads that touch no served cell come back as the file has them (the first 64 KB of each archive)
    for (auto &[a, rs] : table) {
        std::FILE *f = std::fopen((game + "/" + a).c_str(), "rb");
        std::vector<uint8_t> buf(65536), ref;
        size_t got = f ? std::fread(buf.data(), 1, buf.size(), f) : 0;
        ref = buf;
        if (!any_overlap(rs, 0, got)) CHECK(overlay(rs, 0, buf.data(), got) == 0 && buf == ref);
        if (f) std::fclose(f);
    }
    printf("%zu cell(s) read back in %zu reads (%zu patched); %s\n", cells.size(), reads, patched,
           fails ? "FAILED" : "ok");
    return fails ? 1 : 0;
}
