// The client-mod host: loads <overlay dir>/powder_mods/*.dll (powder-wrap copies the mods the player chose with
// POWDER_CLIENT_MODS), gives each the PowderApi (native/sdk/powder_api.h) through its powder_mod_init, and draws
// the Mods tab: the mods, their hooks, their panels.
#pragma once
#include <string>

namespace pw {

// Load the mods once ImGui is up (their panels and powder_imgui_attach need its context). log = the overlay's.
void mods_load(const std::wstring &dir, const std::string &server_url, void (*log)(const char *line));
void mods_draw_tab();

} // namespace pw
