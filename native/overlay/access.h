// Access-finder: "what code writes this address?" Arms a PAGE_GUARD on the page holding a watched address
// and a vectored exception handler; each time the game reads/writes the address, it records the instruction
// (RIP, module+offset), decodes its memory operand with HDE64 to recover the base register + its value +
// displacement (i.e. the struct pointer and field offset), and single-steps past it to re-arm the guard.
//
// This is the bridge from a found value to the code that controls it: the recovered base pointer is the player
// struct, the write RIP is the movement code (which can be NOP'd to freeze/override it). VMProtect guards its
// own CODE pages, not this data page, so the guard works in-process here.
#pragma once
#include <cstdint>
#include <string>
#include <vector>

namespace pw {

struct AccessHit {
    uint64_t rip = 0;
    std::string module;       // module the RIP is in (or "(dynamic)")
    uint64_t module_off = 0;  // rip - module base
    bool write = false;
    int base_reg = -1;        // base register index (0=RAX..15=R15), -1 = RIP-relative / none
    uint64_t base_val = 0;    // base register value at the moment of access (the struct pointer)
    long disp = 0;            // displacement (field offset within the struct)
    uint64_t count = 0;
    uint8_t len = 0;          // instruction length
    bool nopped = false;
    std::string text;         // e.g. "movss [rbx+0x40]" (reads) or "[rbx+0x40] <- " (writes)
};

class AccessFinder {
public:
    bool arm(uint64_t addr, size_t size);   // watch [addr, addr+size)
    void disarm();
    bool armed() const { return armed_; }
    uint64_t target() const { return target_; }

    std::vector<AccessHit> hits() const;     // thread-safe snapshot, most-hit first
    void clear();

    void nop(uint64_t rip);                  // overwrite the instruction at rip with NOPs (disable that write)
    void restore_all();                      // undo all NOPs
    const char *status() const { return status_; }

private:
    bool armed_ = false;
    uint64_t target_ = 0;
    const char *status_ = "";
};

extern AccessFinder g_access;

} // namespace pw
