// live.h - live world edits (live.cpp): the panel's view of them.
#pragma once
#include <cstdint>
#include <string>
#include <vector>
#include "powder_api.h"

namespace live {

struct CellState {
    std::string map, name, mode;   // mode: "same" (fitted into the entry), "grow" (index record redirected)
    std::string state;             // e.g. "waiting for the game to read it", "read 3 times", "not redirected: ..."
    uint64_t reads = 0;
};

struct Status {
    bool hooked = false;           // the file hooks are in
    bool helper = false;           // the session helper's manifest was seen
    int gen = -1, edits = 0;       // its generation, and the newest edit number it reflects
    std::string note;              // why live edits are off, or the last problem
    std::string io;                // how the game reads its archives (event/APC/completion port), once seen
    std::vector<CellState> cells;
};

// Start: hooks ntdll's file reads (when the session helper runs) and polls <dir>live\manifest.tsv. `game_dir` is
// Steep's folder (the archives).
void init(const PowderApi *api, PowderMod *mod, const std::wstring &dir, const std::wstring &game_dir);
Status status();
// The server took an edit: tell the helper to ask for the list now (it asks once a minute otherwise).
void poke();

}  // namespace live
