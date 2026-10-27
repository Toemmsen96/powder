// live_patch.h - the byte-range logic of live world edits (live.cpp), kept free of Windows so that it is unit-tested
// natively (tests/live_patch_test.cpp): a read of [pos, pos + n) of an archive file gets the bytes of every edited
// datafile that overlaps it copied over the file's bytes.
//
// A patch is either "same size" (at its entry's own range: the edited datafile was fitted to exactly the entry's
// size) or "virtual" (beyond the end of the file, where the entry's index record was pointed: any size). A read
// in an archive's virtual area (pos >= vbase) never reaches the file: it is zeros plus the patches.
#pragma once
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

namespace live {

struct Range {
    uint64_t off = 0;              // in the archive file (or its virtual area)
    const uint8_t *data = nullptr;
    size_t size = 0;
};

inline bool overlaps(const Range &r, uint64_t pos, size_t n) {
    return n && r.size && r.off < pos + n && pos < r.off + r.size;
}

inline bool any_overlap(const std::vector<Range> &rs, uint64_t pos, size_t n) {
    for (const Range &r : rs)
        if (overlaps(r, pos, n)) return true;
    return false;
}

// Copy the overlapping parts of `rs` into buf, which holds the `n` bytes of the file from `pos`. Bytes copied.
inline size_t overlay(const std::vector<Range> &rs, uint64_t pos, uint8_t *buf, size_t n) {
    size_t done = 0;
    for (const Range &r : rs) {
        if (!overlaps(r, pos, n)) continue;
        uint64_t a = r.off > pos ? r.off : pos;
        uint64_t b = r.off + r.size < pos + n ? r.off + r.size : pos + n;
        memcpy(buf + (a - pos), r.data + (a - r.off), (size_t)(b - a));
        done += (size_t)(b - a);
    }
    return done;
}

// A read entirely in the virtual area: zeros, then the patches. Returns the bytes "read" (always n).
inline size_t virtual_read(const std::vector<Range> &rs, uint64_t pos, uint8_t *buf, size_t n) {
    memset(buf, 0, n);
    overlay(rs, pos, buf, n);
    return n;
}

// Virtual slots: 1 MiB aligned, past the end of the file with a gap (no aligned read of the file's tail can reach
// into them), each with room to grow by a quarter.
constexpr uint64_t MIB = 1ull << 20;
inline uint64_t align_up(uint64_t v, uint64_t a) { return (v + a - 1) / a * a; }
inline uint64_t virtual_base(uint64_t file_size) { return align_up(file_size, MIB) + 256 * MIB; }
inline uint64_t slot_size(uint64_t data_size) { return align_up(data_size + data_size / 4 + 64 * 1024, MIB); }

}  // namespace live
