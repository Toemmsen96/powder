#include "memscan.h"
#include <windows.h>
#include <cstdio>
#include <cstring>

namespace pw {

size_t Value::size() const {
    switch (type) {
        case ValType::I32: case ValType::F32: return 4;
        case ValType::I64: case ValType::F64: return 8;
    }
    return 4;
}

std::string Value::str() const {
    char buf[64];
    switch (type) {
        case ValType::I32: snprintf(buf, sizeof(buf), "%d", i32); break;
        case ValType::I64: snprintf(buf, sizeof(buf), "%lld", (long long)i64); break;
        case ValType::F32: snprintf(buf, sizeof(buf), "%g", f32); break;
        case ValType::F64: snprintf(buf, sizeof(buf), "%g", f64); break;
    }
    return buf;
}

bool Value::parse(ValType t, const std::string &text, Value &out) {
    out.type = t;
    try {
        switch (t) {
            case ValType::I32: out.i32 = (int32_t)std::stoll(text); break;
            case ValType::I64: out.i64 = (int64_t)std::stoll(text); break;
            case ValType::F32: out.f32 = std::stof(text); break;
            case ValType::F64: out.f64 = std::stod(text); break;
        }
    } catch (...) { return false; }
    return true;
}

bool Value::equals_bytes(const void *p) const {
    switch (type) {
        case ValType::I32: return *(const int32_t *)p == i32;
        case ValType::I64: return *(const int64_t *)p == i64;
        case ValType::F32: { float v = *(const float *)p; float d = v - f32; return d < 0.0001f && d > -0.0001f; }
        case ValType::F64: { double v = *(const double *)p; double d = v - f64; return d < 0.0001 && d > -0.0001; }
    }
    return false;
}

bool MemScan::readable(const void *p, size_t n) const {
    MEMORY_BASIC_INFORMATION mbi;
    if (!VirtualQuery(p, &mbi, sizeof(mbi))) return false;
    if (mbi.State != MEM_COMMIT) return false;
    DWORD prot = mbi.Protect & 0xFF;
    if (prot == PAGE_NOACCESS || (mbi.Protect & PAGE_GUARD)) return false;
    bool ok = prot == PAGE_READONLY || prot == PAGE_READWRITE || prot == PAGE_WRITECOPY ||
              prot == PAGE_EXECUTE_READ || prot == PAGE_EXECUTE_READWRITE || prot == PAGE_EXECUTE_WRITECOPY;
    if (!ok) return false;
    // the slot must not straddle past the region end
    uintptr_t end = (uintptr_t)mbi.BaseAddress + mbi.RegionSize;
    return (uintptr_t)p + n <= end;
}

void MemScan::reset() { matches_.clear(); last_.clear(); }

static const size_t MAX_MATCHES = 2'000'000;   // cap so a first scan can't blow up memory

size_t MemScan::first_scan(const Value &v) {
    reset();
    type_ = v.type;
    size_t step = v.size();
    SYSTEM_INFO si; GetSystemInfo(&si);
    uintptr_t addr = (uintptr_t)si.lpMinimumApplicationAddress;
    uintptr_t max = (uintptr_t)si.lpMaximumApplicationAddress;
    while (addr < max && matches_.size() < MAX_MATCHES) {
        MEMORY_BASIC_INFORMATION mbi;
        if (!VirtualQuery((void *)addr, &mbi, sizeof(mbi))) break;
        uintptr_t base = (uintptr_t)mbi.BaseAddress;
        uintptr_t next = base + mbi.RegionSize;
        DWORD prot = mbi.Protect & 0xFF;
        bool scan = mbi.State == MEM_COMMIT && !(mbi.Protect & PAGE_GUARD) &&
                    (prot == PAGE_READONLY || prot == PAGE_READWRITE || prot == PAGE_WRITECOPY ||
                     prot == PAGE_EXECUTE_READ || prot == PAGE_EXECUTE_READWRITE || prot == PAGE_EXECUTE_WRITECOPY);
        if (scan) {
            for (uintptr_t p = base; p + step <= next && matches_.size() < MAX_MATCHES; p += step) {
                if (v.equals_bytes((void *)p)) {
                    matches_.push_back(p);
                    uint64_t raw = 0; memcpy(&raw, (void *)p, step);
                    last_.push_back(raw);
                }
            }
        }
        addr = next > addr ? next : addr + si.dwPageSize;
    }
    return matches_.size();
}

size_t MemScan::next_scan(Compare cmp, const Value &v) {
    size_t step = v.size();
    std::vector<uintptr_t> keep;
    std::vector<uint64_t> keep_last;
    keep.reserve(matches_.size());
    for (size_t i = 0; i < matches_.size(); ++i) {
        void *p = (void *)matches_[i];
        if (!readable(p, step)) continue;
        uint64_t raw = 0; memcpy(&raw, p, step);
        bool hit = false;
        switch (cmp) {
            case Compare::Exact: hit = v.equals_bytes(p); break;
            case Compare::Changed: hit = raw != last_[i]; break;
            case Compare::Unchanged: hit = raw == last_[i]; break;
            case Compare::Greater:
            case Compare::Less: {
                double cur, prev;
                if (type_ == ValType::F32) { float a; memcpy(&a, &raw, 4); cur = a; float b; memcpy(&b, &last_[i], 4); prev = b; }
                else if (type_ == ValType::F64) { double a; memcpy(&a, &raw, 8); cur = a; double b; memcpy(&b, &last_[i], 8); prev = b; }
                else if (type_ == ValType::I32) { cur = (double)(int32_t)raw; prev = (double)(int32_t)last_[i]; }
                else { cur = (double)(int64_t)raw; prev = (double)(int64_t)last_[i]; }
                hit = (cmp == Compare::Greater) ? (cur > prev) : (cur < prev);
                break;
            }
        }
        if (hit) { keep.push_back(matches_[i]); keep_last.push_back(raw); }
    }
    matches_.swap(keep);
    last_.swap(keep_last);
    return matches_.size();
}

bool MemScan::read(size_t i, Value &out) const {
    if (i >= matches_.size()) return false;
    void *p = (void *)matches_[i];
    size_t step = (type_ == ValType::I64 || type_ == ValType::F64) ? 8 : 4;
    if (!readable(p, step)) return false;
    out.type = type_;
    memcpy(&out.i64, p, step);   // covers all 4/8-byte types via the union
    return true;
}

void MemScan::write(uintptr_t addr, const Value &v) {
    void *p = (void *)addr;
    size_t n = v.size();
    DWORD old;
    if (!VirtualProtect(p, n, PAGE_EXECUTE_READWRITE, &old)) return;
    memcpy(p, &v.i64, n);
    DWORD tmp;
    VirtualProtect(p, n, old, &tmp);
}

void MemScan::apply_frozen() {
    for (auto &f : frozen) write(f.addr, f.value);
}

} // namespace pw
