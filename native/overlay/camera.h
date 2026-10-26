// The third-person camera's position, found live each session (pointer chains to it don't survive a restart:
// the copies one finds are per-frame scratch). The camera looks at the rider, so its position gives the view
// direction (camera -> rider). Calibration, on a background thread: snapshot every float3 1.5-15 m from the
// player, then, while the player stands still and the camera turns, keep the points that moved each time at a
// constant distance (they orbit the player); the position most of them agree on is the camera. Same method as
// tools/trainer/findcam.py. Reads are fault-safe (ReadProcessMemory on ourselves).
#pragma once
#include <cstdint>
#include <string>

namespace pw {

void camera_calibrate(uintptr_t player_pos);   // start (or restart) a calibration; returns at once
// The camera's position (address of its float3), 0 while unknown. Call every frame: it drives the calibration
// and drops a camera that drifted away from the player (map change, respawn).
uintptr_t camera_pos(uintptr_t player_pos);
const char *camera_status();

} // namespace pw
