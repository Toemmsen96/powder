// Game fixes: crash guards for this game build, from powder_trainer.json's "fixes", applied by the overlay at
// start. A null guard is a mid hook just before an instruction that dereferences a register the game did not
// check: if that register is NULL it is pointed at a zeroed dummy block instead (the read yields 0), and the
// first few times are logged with the registers, to find out what was missing. Each site is checked against its
// original bytes first (another build -> the fix is skipped, not misapplied). Optional "args_at" = a mid hook at
// the call whose NULL result is guarded, recording its rcx/rdx (the lookup's inputs) for the log line.
#pragma once
#include <string>

namespace pw {

void fixes_apply(const wchar_t *trainer_json, void (*log)(const char *line));

} // namespace pw
