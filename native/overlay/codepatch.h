// Writing the game's code and allocating near it - shared by noclip and the hook engine.
//
// VMProtect blocks the game process's own writes to its code (VirtualProtect, and WriteProcessMemory on itself,
// fail in Steep), so write_code falls back to armer.exe --write (the TLS helper beside the overlay), which
// patches from outside with VirtualProtectEx + WriteProcessMemory. Code outside the game's exe (system DLLs) is
// written in-process.
#pragma once
#include <cstdint>
#include <cstddef>
#include <string>

namespace pw {

void code_set_helper(const wchar_t *armer_exe);
// Write n bytes of code at `at` (verified by reading back); on failure `how` says what each way returned.
bool write_code(uintptr_t at, const uint8_t *src, size_t n, std::string &how);
// Committed RWX memory within +-2 GB of target (reachable by a rel32 jmp), or nullptr.
uint8_t *alloc_near(uintptr_t target, size_t size);

} // namespace pw
