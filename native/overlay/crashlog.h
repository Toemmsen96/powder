// Crash log: Steep keeps no crash report under Wine and Proton's WINEDEBUG hides Wine's own, so when the game dies
// with our code involved the overlay's log has to say where.
//  - A vectored handler logs faults whose address is in our code (the overlay, client mods, hook trampolines,
//    mid-hook stubs, noclip caves) and passes them on: VMProtect raises exceptions on purpose, nothing is swallowed.
//  - An unhandled-exception filter, chained to the game's, logs whatever kills the game: code, module+offset,
//    the faulting access, registers and the return addresses found on the stack.
// Both write with a fixed buffer and raw file calls (the heap may be what broke).
#pragma once
#include <cstdint>

namespace pw {

void crashlog_install(const wchar_t *logpath);          // once, after the game set up its own handlers
void crashlog_add_range(uintptr_t lo, uintptr_t hi);    // our dynamic code: trampolines, stubs, caves

} // namespace pw
