// Player position + noclip for the Trainer tab, driven by powder_trainer.json beside the DLL (powder-wrap deploys
// native/overlay/steep_trainer.json): for this game build, the pointer chains from static addresses to the
// player's transform (found with tools/trainer/ptrscan.py) and the instructions that write its position (found
// with tools/trainer/watch.py, a perf watchpoint from outside the game).
//
// Noclip: each writer gets a 5-byte jmp to a cave in our own memory near the module; the cave lea's the write's
// address and skips the write when it falls in [lo, hi) - the player's position - so physics and collision can
// no longer move the player and Fly moves it freely. Everything else that shares the code (hundreds of objects)
// runs as before. Same design as tools/trainer/skiphook.py, in-process. The site is checked against the bytes in
// the JSON before it is patched (a game update -> refuse, don't guess).
#pragma once
#include <cstdint>
#include <string>

namespace pw {

bool trainer_load(const wchar_t *path, std::string &err);   // read powder_trainer.json
bool trainer_loaded();

// The player's position (address of a float3) through the chains; 0 if none resolves. `why` says which.
uintptr_t find_player_pos(std::string &why);
// The third-person camera's position (float3) through the "camera" chains, checked against the player's; 0 if none.
uintptr_t find_camera_pos(uintptr_t player_pos, std::string &why);

bool noclip_install(std::string &err);     // hook every writer (once); idle until a range is set
bool noclip_installed();
void noclip_set_range(uintptr_t lo, uintptr_t hi);   // skip the game's writes into [lo, hi); (0, 0) = off
bool noclip_active();

} // namespace pw
