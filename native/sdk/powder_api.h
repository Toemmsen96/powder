/* Powder client-mod API: what a mod DLL loaded into Steep by the Powder overlay can do.
 *
 * A client mod is a DLL (mods/<name>/client/<name>.dll, built from mods/<name>/src/) that exports
 *
 *     POWDER_EXPORT int powder_mod_init(const PowderApi *api, PowderMod *mod);   // 0 = ok
 *
 * The overlay (native/overlay/, the mod host) calls it once, after the game's code is unpacked and the overlay is
 * drawing. Keep `api` and `mod`: they live as long as the game. Everything here is called from the game's
 * threads; hooks run on whatever thread the game runs the hooked code on.
 *
 * Hooks: `hook` detours a function (the classic: your detour runs instead, call `*original` for the game's);
 * `hook_mid` runs a callback at any instruction with all registers in a PowderRegs it may change, then the
 * original instructions run. Steep's code is protected by VMProtect, which blocks the game process's own writes
 * to its code: the overlay then patches from outside (armer.exe), so installing a hook on game code takes a few
 * milliseconds. Windows/system DLL functions are hooked in-process. Addresses: module_base + RVA (stable for
 * one game build) or find_pattern (survives small updates).
 *
 * Every frame: on_frame runs a callback once per rendered frame (the game's render thread: keep it short).
 * Server: http sends a request to the Powder server this game is connected to, only to /_powder/... paths (a
 * server mod answers them with api.before_http in Lua: client and server halves of one mod talk this way). It
 * carries no session of the player or the admin; the reply comes to `done` on the mod host's network thread.
 *
 * UI: add_panel draws inside the overlay's Mods tab. To use Dear ImGui, compile it into your mod (the same
 * version as the overlay, POWDER_IMGUI_VERSION) and call powder_imgui_attach(api) (C++, below) once.
 *
 * Trust: a client mod is native code with the player's rights. The player decides which ones load
 * (powder-wrap: POWDER_CLIENT_MODS; the Windows launcher: per-server approval pinned to the files' hashes).
 */
#ifndef POWDER_API_H
#define POWDER_API_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define POWDER_API_VERSION 2
#define POWDER_IMGUI_VERSION "1.91.5"
#define POWDER_EXPORT __declspec(dllexport)

/* The registers at a mid-function hook. Change any of them (except rsp) and the game continues with yours.
 * xmm0-xmm5 are the volatile vector registers (float arguments, SIMD temporaries). */
typedef struct PowderRegs {
    uint8_t  xmm[6][16];
    uint64_t rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi;
    uint64_t r8, r9, r10, r11, r12, r13, r14, r15;
    uint64_t rflags;
} PowderRegs;

typedef struct PowderMod {
    const char *name;      /* the mod's folder name, set by the host */
    void *user;            /* yours */
} PowderMod;

typedef void (*PowderMidFn)(PowderRegs *regs, void *user);
typedef void (*PowderPanelFn)(void *user);
typedef void (*PowderFrameFn)(void *user);
typedef void (*PowderHttpFn)(int status, const char *body, size_t len, void *user);   /* status 0: no reply */

typedef struct PowderApi {
    uint32_t version;      /* POWDER_API_VERSION of the host; check >= what you need */
    uint32_t size;         /* sizeof(PowderApi) of the host: newer hosts only append */

    /* powder_overlay.log, prefixed with the mod's name */
    void (*log)(const PowderMod *mod, const char *fmt, ...);

    /* memory */
    uintptr_t (*module_base)(const char *module);                  /* NULL = the game's exe */
    uintptr_t (*find_pattern)(const char *module, const char *pattern);   /* "48 8B ?? ?? 0F 29", 0 if none */
    int (*read)(uintptr_t addr, void *out, size_t n);              /* 0 = ok; never faults */
    int (*write)(uintptr_t addr, const void *src, size_t n);       /* data; 0 = ok */
    int (*patch_code)(uintptr_t addr, const void *bytes, size_t n);   /* code (through armer.exe if needed) */

    /* hooks: 0 = ok, else see last_error. id (optional) identifies the hook for unhook. */
    int (*hook)(const PowderMod *mod, uintptr_t target, void *detour, void **original, int *id);
    int (*hook_mid)(const PowderMod *mod, uintptr_t at, PowderMidFn fn, void *user, int *id);
    int (*unhook)(int id);
    const char *(*last_error)(void);

    /* UI: a section in the overlay's Mods tab, drawn every frame while the menu is open */
    void (*add_panel)(const PowderMod *mod, const char *title, PowderPanelFn draw, void *user);
    void *imgui_context;   /* ImGuiContext* */
    void *(*imgui_alloc)(size_t size, void *user);
    void (*imgui_free)(void *ptr, void *user);

    /* game */
    uintptr_t (*player_position)(void);    /* address of the player's float3 (x, y, z; z is up), 0 if unknown */
    const char *server_url;                 /* the Powder server, e.g. "https://public-ubiservices.ubi.com" */

    /* version 2 */
    void (*on_frame)(const PowderMod *mod, PowderFrameFn fn, void *user);
    /* method "GET"/"POST"/...; path must start with "/_powder/"; body may be NULL. 0 = queued. */
    int (*http)(const PowderMod *mod, const char *method, const char *path, const char *body, PowderHttpFn done,
                void *user);
} PowderApi;

typedef int (*PowderModInitFn)(const PowderApi *api, PowderMod *mod);

#ifdef __cplusplus
}

#ifdef IMGUI_VERSION
/* Use the overlay's Dear ImGui context from this DLL (call once in powder_mod_init). */
inline bool powder_imgui_attach(const PowderApi *api) {
    if (__builtin_strcmp(IMGUI_VERSION, POWDER_IMGUI_VERSION) != 0 || !api->imgui_context) return false;
    ImGui::SetAllocatorFunctions(api->imgui_alloc, api->imgui_free, nullptr);
    ImGui::SetCurrentContext((ImGuiContext *)api->imgui_context);
    return true;
}
#endif
#endif

#endif /* POWDER_API_H */
