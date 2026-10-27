// Function and mid-function hooks, for the overlay and for client mods (the mod API's hook / hook_mid).
//
// MinHook builds the trampoline (its CreateTrampolineFunction relocates the target's first instructions, rip-
// relative operands and short jumps included, into a buffer near the target, plus a relay jmp to the detour);
// we write the 5-byte jmp ourselves with write_code, because MinHook's VirtualProtect is blocked on Steep's code
// (codepatch.h: armer.exe patches from outside). A mid hook's detour is a register-saving stub that calls the
// callback with a PowderRegs, restores the (possibly changed) registers and continues in the trampoline.
//
// Threads are not suspended while patching (a suspended thread could hold the heap lock armer.exe's start needs):
// a thread executing exactly the 5 patched bytes at that moment is the remaining risk.
#pragma once
#include <cstdint>
#include <string>
#include <vector>

struct PowderRegs;

namespace pw {

// id > 0, or 0 with err set. original = the trampoline that runs the game's function.
int hook_function(const std::string &owner, uintptr_t target, void *detour, void **original, std::string &err);
int hook_mid(const std::string &owner, uintptr_t at, void (*fn)(PowderRegs *, void *), void *user, std::string &err);
bool unhook(int id, std::string &err);      // restores the original bytes (buffers stay: a thread may be inside)

struct HookInfo { int id; std::string owner, kind; uintptr_t target; bool enabled; };
std::vector<HookInfo> hook_list();

} // namespace pw
