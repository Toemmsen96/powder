// A small, generic in-process memory scanner (Cheat-Engine style) for the overlay's Debug tab. It needs no
// knowledge of Steep: it scans the game's own address space for a value, refines on repeated scans, and can
// edit or freeze a found address. This is the tool that makes finding game values (for future trainer
// features) possible without reverse-engineering the binary up front.
#pragma once
#include <cstdint>
#include <vector>
#include <string>

namespace pw {

enum class ValType { I32, I64, F32, F64 };

struct Value {
    ValType type = ValType::I32;
    union {
        int32_t i32;
        int64_t i64;
        float f32;
        double f64;
    };
    size_t size() const;
    std::string str() const;
    static bool parse(ValType t, const std::string &text, Value &out);
    bool equals_bytes(const void *p) const;   // does memory at p hold this value?
};

enum class Compare { Exact, Changed, Unchanged, Greater, Less };

struct Frozen {
    uintptr_t addr;
    Value value;
};

class MemScan {
public:
    // First scan: find every readable aligned slot equal to `v`. Returns match count.
    size_t first_scan(const Value &v);
    // Refine: keep matches that satisfy cmp (Exact uses `v`; others compare against the last read).
    size_t next_scan(Compare cmp, const Value &v);
    void reset();

    size_t count() const { return matches_.size(); }
    ValType type() const { return type_; }
    // read the current value at match i (false if unreadable); fills out.type = scan type
    bool read(size_t i, Value &out) const;
    uintptr_t addr(size_t i) const { return i < matches_.size() ? matches_[i] : 0; }
    void write(uintptr_t addr, const Value &v);   // poke a value into the process

    // freeze list (rewritten each frame by the overlay)
    std::vector<Frozen> frozen;
    void apply_frozen();

private:
    ValType type_ = ValType::I32;
    std::vector<uintptr_t> matches_;
    std::vector<uint64_t> last_;   // last-seen raw bytes per match, for Changed/Unchanged/Greater/Less
    bool readable(const void *p, size_t n) const;
};

} // namespace pw
