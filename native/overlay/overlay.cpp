// Powder in-game overlay: a Dear ImGui menu drawn inside Steep by hooking the D3D11 swap chain's Present.
//
// How it loads: the version.dll proxy (native/version_proxy.c) LoadLibrary's this DLL once Steep's code is
// unpacked (powder-wrap deploys it beside version.dll when POWDER_OVERLAY=1). Steep renders with D3D11 under
// Proton's DXVK, so we create a throwaway swap chain to read IDXGISwapChain's vtable, MinHook its Present and
// ResizeBuffers, and draw on top. Toggle with HOME.
//
// What it shows:
//   Server tab - the same admin actions as the web page (/_powder/admin): sign in, players, grant
//                credits/items, edit stats, reset, broadcast. Talks to /_powder/admin/api/* over HTTPS
//                (http.cpp); needs the Rust server (the admin mod is Lua -> Rust only).
//   Debug tab  - connection status, the broadcast feed, FPS, an HTTP activity log, and a generic in-process
//                memory scanner (memscan.cpp) - no Steep reverse-engineering needed.
//   Trainer tab- address slots (freeze/set), Fly, Teleport; the player's position found through
//                powder_trainer.json's pointer chains, and noclip while flying (noclip.cpp).
//   Mods tab   - the client mods it hosts (modhost.cpp: powder_mods/*.dll, native/sdk/powder_api.h), their
//                hooks (hooks.cpp) and panels.
//
// All network calls run on a worker thread; the Present/UI thread never blocks on I/O. Diagnostics go to
// powder_overlay.log beside the DLL.
//
//   x86_64-w64-mingw32-g++ -O2 ... (see native/build.sh)

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <cstdarg>
#include <cstdio>
#include <string>
#include <vector>
#include <deque>
#include <thread>
#include <mutex>
#include <condition_variable>
#include <functional>
#include <atomic>

#include "MinHook.h"
#include "imgui.h"
#include "backends/imgui_impl_dx11.h"
#include "backends/imgui_impl_win32.h"
#include "json.hpp"
#include "http.h"
#include "memscan.h"
#include "access.h"
#include "noclip.h"
#include "codepatch.h"
#include "modhost.h"
#include "crashlog.h"
#include "fixes.h"

using json = nlohmann::json;

extern IMGUI_IMPL_API LRESULT ImGui_ImplWin32_WndProcHandler(HWND hWnd, UINT msg, WPARAM wParam, LPARAM lParam);

// ---- paths / logging -------------------------------------------------------------------------------------
static wchar_t g_dir[MAX_PATH];
static wchar_t g_logpath[MAX_PATH];
static wchar_t g_cfgpath[MAX_PATH];

static void olog(const char *fmt, ...) {
    FILE *f = _wfopen(g_logpath, L"a");
    if (!f) return;
    SYSTEMTIME t; GetLocalTime(&t);
    char msg[512];
    va_list ap; va_start(ap, fmt); vsnprintf(msg, sizeof(msg), fmt, ap); va_end(ap);
    fprintf(f, "%02d:%02d:%02d.%03d overlay: %s\r\n", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, msg);
    fclose(f);
}

// Config file beside the DLL: line 1 = base URL; optional lines `show=1` (force the menu visible, for the
// headless harness) and `login=<email>:<password>` (auto sign-in, harness only).
static std::string g_base_url = "https://public-ubiservices.ubi.com";
static bool g_cfg_show = false;
static std::string g_cfg_login_email, g_cfg_login_pass;

static void read_cfg() {
    FILE *f = _wfopen(g_cfgpath, L"rb");
    if (!f) return;
    char line[1024];
    int n = 0;
    while (fgets(line, sizeof(line), f)) {
        std::string s(line);
        while (!s.empty() && (s.back() == '\n' || s.back() == '\r' || s.back() == ' ')) s.pop_back();
        if (s.empty()) continue;
        if (n == 0 && s.rfind("http", 0) == 0) { g_base_url = s; }
        else if (s == "show=1") { g_cfg_show = true; }
        else if (s.rfind("login=", 0) == 0) {
            std::string rest = s.substr(6);
            size_t c = rest.find(':');
            if (c != std::string::npos) { g_cfg_login_email = rest.substr(0, c); g_cfg_login_pass = rest.substr(c + 1); }
        }
        ++n;
    }
    fclose(f);
}

// ---- application state (shared UI thread <-> net thread, guarded by st.m) ---------------------------------
struct Player {
    std::string name, profile_id, email, created_at, level;
    bool admin = false, online = false;
    long long credits = 0;
};
struct Item { std::string item_id, last_modified; long long quantity = 0; };
struct Stat { std::string key; double value = 0; bool is_double = false; };
struct Detail {
    std::string profile_id;
    long long credits = 0;
    std::vector<Item> items;
    std::vector<Stat> stats;
    bool loaded = false;
};

struct AppState {
    std::mutex m;
    bool logged_in = false;
    std::string who;
    std::string status = "not signed in";
    std::vector<Player> players;
    Detail detail;
    std::vector<std::string> feed;
    long long feed_last = 0;
    std::vector<std::string> httplog;
    std::atomic<bool> busy{false};
};
static AppState g_st;

static void push_log(AppState &st, const std::string &s) {
    olog("%s", s.c_str());   // mirror to the file log so failures are visible without the Debug tab
    std::lock_guard<std::mutex> lk(st.m);
    st.status = s;
    st.httplog.push_back(s);
    if (st.httplog.size() > 200) st.httplog.erase(st.httplog.begin());
}

// ---- net worker ------------------------------------------------------------------------------------------
class Net {
public:
    explicit Net(const std::string &base) : client_(base) {}
    void start() { thread_ = std::thread([this] { run(); }); }
    void stop() {
        { std::lock_guard<std::mutex> lk(m_); quit_ = true; }
        cv_.notify_all();
        if (thread_.joinable()) thread_.join();
    }
    void enqueue(std::function<void(pw::HttpClient &, AppState &)> job) {
        { std::lock_guard<std::mutex> lk(m_); q_.push_back(std::move(job)); }
        cv_.notify_all();
    }
    bool valid() const { return client_.valid(); }
    std::string host() const { return client_.host(); }

private:
    void run() {
        for (;;) {
            std::function<void(pw::HttpClient &, AppState &)> job;
            {
                std::unique_lock<std::mutex> lk(m_);
                cv_.wait(lk, [&] { return quit_ || !q_.empty(); });
                if (quit_ && q_.empty()) return;
                job = std::move(q_.front());
                q_.pop_front();
            }
            g_st.busy = true;
            try { job(client_, g_st); } catch (const std::exception &e) { push_log(g_st, std::string("job error: ") + e.what()); }
            g_st.busy = false;
        }
    }
    pw::HttpClient client_;
    std::thread thread_;
    std::mutex m_;
    std::condition_variable cv_;
    std::deque<std::function<void(pw::HttpClient &, AppState &)>> q_;
    bool quit_ = false;
};
static Net *g_net = nullptr;

// Extract a human error message from an API error body, else fall back to the status code.
static std::string api_err(const pw::HttpResponse &r) {
    if (r.status == 0) return r.error;
    try {
        auto j = json::parse(r.body);
        if (j.contains("error")) return j["error"].get<std::string>();
        if (j.contains("message")) return j["message"].get<std::string>();
    } catch (...) {}
    return "HTTP " + std::to_string(r.status);
}

// ---- jobs ------------------------------------------------------------------------------------------------
static void job_players(pw::HttpClient &c, AppState &st) {
    auto r = c.get("/_powder/admin/api/players");
    if (!r.ok()) { push_log(st, "players: " + api_err(r)); return; }
    olog("players request ok (HTTP %d)", r.status);
    try {
        auto j = json::parse(r.body);
        std::vector<Player> ps;
        for (auto &p : j["players"]) {
            Player x;
            x.name = p.value("name", "");
            x.profile_id = p.value("profile_id", "");
            x.email = p.value("email", "");
            x.created_at = p.value("created_at", "");
            x.admin = p.value("admin", false);
            x.online = p.value("connect", false) || p.value("game", false);
            if (p.contains("credits") && p["credits"].is_number()) x.credits = p["credits"].get<long long>();
            // the server sends level as a string (or null) - not a number
            if (p.contains("level") && p["level"].is_string()) x.level = p["level"].get<std::string>();
            else if (p.contains("level") && p["level"].is_number()) x.level = std::to_string(p["level"].get<long long>());
            else x.level = "-";
            ps.push_back(std::move(x));
        }
        std::lock_guard<std::mutex> lk(st.m);
        st.players.swap(ps);
        st.status = std::to_string(st.players.size()) + " player(s)";
    } catch (const std::exception &e) { push_log(st, std::string("players parse: ") + e.what()); }
}

static void job_detail(pw::HttpClient &c, AppState &st, std::string id) {
    auto r = c.get("/_powder/admin/api/player?id=" + id);
    if (!r.ok()) { push_log(st, "player: " + api_err(r)); return; }
    try {
        auto j = json::parse(r.body);
        Detail d;
        d.profile_id = id;
        d.credits = j["inventory"].value("credits", (long long)0);
        for (auto &it : j["inventory"]["items"]) {
            Item x; x.item_id = it.value("item_id", ""); x.quantity = it.value("quantity", (long long)0);
            x.last_modified = it.value("last_modified", "");
            d.items.push_back(std::move(x));
        }
        for (auto &s : j["stats"]) {
            Stat x; x.key = s.value("key", ""); x.is_double = s.value("double", false);
            if (s.contains("value") && s["value"].is_number()) x.value = s["value"].get<double>();
            d.stats.push_back(std::move(x));
        }
        d.loaded = true;
        std::lock_guard<std::mutex> lk(st.m);
        st.detail = std::move(d);
    } catch (const std::exception &e) { push_log(st, std::string("player parse: ") + e.what()); }
}

static void job_feed(pw::HttpClient &c, AppState &st) {
    long long since;
    { std::lock_guard<std::mutex> lk(st.m); since = st.feed_last; }
    auto r = c.get("/_powder/admin/api/feed?since=" + std::to_string(since));
    if (!r.ok()) return;   // feed is best-effort; stay quiet on failure
    try {
        auto j = json::parse(r.body);
        std::lock_guard<std::mutex> lk(st.m);
        for (auto &e : j["entries"]) {
            std::string from = e.value("from", "?");
            std::string msg = e.value("message", "");
            st.feed.push_back(from + ": " + msg);
        }
        if (st.feed.size() > 100) st.feed.erase(st.feed.begin(), st.feed.begin() + (st.feed.size() - 100));
        st.feed_last = j.value("last", since);
    } catch (...) {}
}

// ---- D3D11 / ImGui ---------------------------------------------------------------------------------------
typedef HRESULT(STDMETHODCALLTYPE *Present_t)(IDXGISwapChain *, UINT, UINT);
typedef HRESULT(STDMETHODCALLTYPE *ResizeBuffers_t)(IDXGISwapChain *, UINT, UINT, UINT, DXGI_FORMAT, UINT);
static Present_t g_present_orig = nullptr;
static ResizeBuffers_t g_resize_orig = nullptr;

static ID3D11Device *g_dev = nullptr;
static ID3D11DeviceContext *g_ctx = nullptr;
static ID3D11RenderTargetView *g_rtv = nullptr;
static HWND g_hwnd = nullptr;
static WNDPROC g_wndproc = nullptr;
static bool g_init = false;
static std::atomic<bool> g_show{false};
static bool g_autologin_done = false;

static pw::MemScan g_scan;
static std::atomic<bool> g_scanning{false};

// ---- Trainer: addresses the user found (via the scanner), with freeze, fly and teleport ------------------
struct Slot {
    char label[32];
    char input[32];       // the "set value" field (persists across frames)
    uintptr_t addr;
    pw::ValType type;
    bool freeze;
    pw::Value frozen;     // value held while freeze is on
};
static std::vector<Slot> g_slots;
static int g_axis[3] = {-1, -1, -1};    // slot indices used as X, Y, Z for fly / teleport
static bool g_fly = false;
static float g_fly_speed = 2.0f;
static bool g_have_spot = false;
static pw::Value g_spot[3];

static void add_slot(const char *label, uintptr_t addr, pw::ValType type) {
    Slot s{};
    strncpy(s.label, label, sizeof(s.label) - 1);
    s.addr = addr; s.type = type; s.freeze = false;
    pw::peek(addr, type, s.frozen);     // seed the hold value with the current one
    g_slots.push_back(s);
}

static void erase_slot(int i) {
    for (int k = 0; k < 3; ++k) { if (g_axis[k] == i) g_axis[k] = -1; else if (g_axis[k] > i) --g_axis[k]; }
    g_slots.erase(g_slots.begin() + i);
}

// ---- the player's position (powder_trainer.json's chains) and noclip while flying (noclip.cpp) -------------
// The position moves to new memory on a map change, a respawn and every launch, so it is looked up again
// every frame while flying (Fly must never write into memory the game has freed) and every second otherwise.
static uintptr_t g_player_pos = 0;      // the position the "player X/Y/Z" slots point at; 0 = none
static std::string g_player_why = "not looked up yet", g_noclip_err;
static bool g_noclip_with_fly = true;
static DWORD g_player_checked = 0;

static void set_fly(bool on);

static void drop_player_slots() {
    for (int i = (int)g_slots.size() - 1; i >= 0; --i)
        if (strncmp(g_slots[i].label, "player ", 7) == 0) erase_slot(i);
    g_player_pos = 0;
}

static void track_player() {
    if (!pw::trainer_loaded()) return;
    uintptr_t pos = pw::find_player_pos(g_player_why);
    if (pos == g_player_pos) return;
    bool flying = g_fly;
    if (flying) set_fly(false);
    drop_player_slots();
    if (!pos) { olog("player position lost (%s)", g_player_why.c_str()); return; }
    static const char *names[3] = {"player X", "player Y", "player Z"};
    for (int k = 0; k < 3; ++k) { add_slot(names[k], pos + 4 * k, pw::ValType::F32); g_axis[k] = (int)g_slots.size() - 1; }
    g_player_pos = pos;
    olog("player position at %p (%s)", (void *)pos, g_player_why.c_str());
    if (flying) set_fly(true);
}

static void set_fly(bool on) {
    g_fly = on;
    if (!pw::trainer_loaded()) return;
    if (on && g_noclip_with_fly && g_axis[0] >= 0) {
        if (!pw::noclip_installed()) {
            if (pw::noclip_install(g_noclip_err)) { g_noclip_err.clear(); olog("noclip hooks installed"); }
            else { olog("noclip: %s", g_noclip_err.c_str()); return; }
        }
        uintptr_t x = g_slots[g_axis[0]].addr;
        pw::noclip_set_range(x, x + 16);    // the position is written as one 16-byte vector
    } else {
        pw::noclip_set_range(0, 0);
    }
}

static double slot_as_double(const pw::Value &v) {
    switch (v.type) {
        case pw::ValType::F32: return v.f32;
        case pw::ValType::F64: return v.f64;
        case pw::ValType::I32: return v.i32;
        case pw::ValType::I64: return (double)v.i64;
    }
    return 0;
}
static pw::Value double_as(pw::ValType t, double d) {
    pw::Value v; v.type = t;
    switch (t) {
        case pw::ValType::F32: v.f32 = (float)d; break;
        case pw::ValType::F64: v.f64 = d; break;
        case pw::ValType::I32: v.i32 = (int)d; break;
        case pw::ValType::I64: v.i64 = (long long)d; break;
    }
    return v;
}

// Run every frame: keep the player's position current, hold frozen slots, and (when fly is on) move the X/Y/Z
// slots with W/A/S/D + Shift/Ctrl, overwriting the game's physics for those addresses. F6 toggles Fly.
static void trainer_apply() {
    DWORD now = GetTickCount();
    bool due = g_player_pos ? (g_fly || now - g_player_checked > 1000)          // follow it (map change, respawn)
                            : (g_axis[0] < 0 && now - g_player_checked > 2000);  // find it, unless slots are manual
    if (due && pw::trainer_loaded()) { g_player_checked = now; track_player(); }
    static bool f6_was = false;
    bool f6 = (GetAsyncKeyState(VK_F6) & 0x8000) != 0;
    if (f6 && !f6_was) {
        if (!g_player_pos && pw::trainer_loaded()) track_player();
        if (g_axis[0] >= 0 && g_axis[1] >= 0 && g_axis[2] >= 0) set_fly(!g_fly);
    }
    f6_was = f6;
    for (auto &s : g_slots)
        if (s.freeze) pw::poke(s.addr, s.frozen);

    bool axes_ok = g_axis[0] >= 0 && g_axis[1] >= 0 && g_axis[2] >= 0 &&
                   g_axis[0] < (int)g_slots.size() && g_axis[1] < (int)g_slots.size() &&
                   g_axis[2] < (int)g_slots.size();
    if (!g_fly || !axes_ok) return;
    auto held = [](int vk) { return (GetAsyncKeyState(vk) & 0x8000) != 0; };
    double mv[3] = {0, 0, 0};
    // Steep's world: Z is up. WASD = horizontal (X = A/D, Y = W/S), Shift/Ctrl = up/down (Z). World axes, not
    // the camera's - camera-relative needs the view matrix (later).
    if (held('D')) mv[0] += 1;
    if (held('A')) mv[0] -= 1;
    if (held('W')) mv[1] += 1;
    if (held('S')) mv[1] -= 1;
    if (held(VK_SHIFT)) mv[2] += 1;
    if (held(VK_CONTROL)) mv[2] -= 1;
    for (int k = 0; k < 3; ++k) {
        Slot &s = g_slots[g_axis[k]];
        pw::Value cur;
        if (!pw::peek(s.addr, s.type, cur)) continue;
        pw::Value nv = double_as(s.type, slot_as_double(cur) + mv[k] * g_fly_speed);
        pw::poke(s.addr, nv);     // writing every frame (even with no key) holds position = hover/fly
    }
}

static void create_rtv(IDXGISwapChain *sc) {
    ID3D11Texture2D *back = nullptr;
    if (SUCCEEDED(sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&back)) && back) {
        g_dev->CreateRenderTargetView(back, nullptr, &g_rtv);
        back->Release();
    }
}

static LRESULT CALLBACK hk_wndproc(HWND h, UINT msg, WPARAM w, LPARAM l) {
    if (msg == WM_KEYDOWN && w == VK_HOME) { g_show = !g_show.load(); return 0; }
    if (g_init && g_show.load()) {
        ImGui_ImplWin32_WndProcHandler(h, msg, w, l);
        ImGuiIO &io = ImGui::GetIO();
        if (io.WantCaptureMouse && msg >= WM_MOUSEFIRST && msg <= WM_MOUSELAST) return 1;
        if (io.WantCaptureKeyboard &&
            (msg == WM_KEYDOWN || msg == WM_KEYUP || msg == WM_CHAR || msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP))
            return 1;
    }
    return CallWindowProcW(g_wndproc, h, msg, w, l);
}

static void draw_ui();   // below
static void olog_line(const char *line) { olog("%s", line); }

static void init_imgui(IDXGISwapChain *sc) {
    if (FAILED(sc->GetDevice(__uuidof(ID3D11Device), (void **)&g_dev)) || !g_dev) {
        olog("GetDevice failed; cannot init");
        return;
    }
    g_dev->GetImmediateContext(&g_ctx);
    DXGI_SWAP_CHAIN_DESC desc;
    sc->GetDesc(&desc);
    g_hwnd = desc.OutputWindow;
    create_rtv(sc);

    IMGUI_CHECKVERSION();
    ImGui::CreateContext();
    ImGui::StyleColorsDark();
    ImGuiIO &io = ImGui::GetIO();
    io.IniFilename = nullptr;
    ImGui_ImplWin32_Init(g_hwnd);
    ImGui_ImplDX11_Init(g_dev, g_ctx);

    if (g_hwnd) g_wndproc = (WNDPROC)SetWindowLongPtrW(g_hwnd, GWLP_WNDPROC, (LONG_PTR)hk_wndproc);
    g_init = true;
    if (g_cfg_show) g_show = true;
    olog("ImGui initialized (hwnd=%p, %ux%u)", (void *)g_hwnd, desc.BufferDesc.Width, desc.BufferDesc.Height);
}

static HRESULT STDMETHODCALLTYPE hk_present(IDXGISwapChain *sc, UINT sync, UINT flags) {
    if (!g_init) init_imgui(sc);
    static bool mods_loaded = false;
    if (g_init && !mods_loaded) {
        mods_loaded = true;
        pw::crashlog_install(g_logpath);      // after the game's own handlers, so ours chains to them
        wchar_t trainer[MAX_PATH];
        _snwprintf(trainer, MAX_PATH, L"%spowder_trainer.json", g_dir);
        pw::fixes_apply(trainer, olog_line);
        pw::mods_load(g_dir, g_base_url, olog_line);
    }
    if (g_init && g_rtv) {
        // harness: auto sign-in + load players once, so a headless run can verify the whole path
        if (!g_autologin_done && !g_cfg_login_email.empty() && g_net && g_net->valid()) {
            g_autologin_done = true;
            std::string email = g_cfg_login_email, pass = g_cfg_login_pass;
            g_net->enqueue([email, pass](pw::HttpClient &c, AppState &st) {
                json b = {{"email", email}, {"password", pass}};
                auto r = c.post("/_powder/admin/api/login", b.dump());
                if (r.ok()) {
                    std::string name;
                    try { auto j = json::parse(r.body); name = j.value("name", ""); std::lock_guard<std::mutex> lk(st.m);
                          st.logged_in = true; st.who = name; st.status = "signed in as " + name; } catch (...) {}
                    job_players(c, st);
                    olog("auto-login ok (signed in as %s); players fetched", name.c_str());
                } else { push_log(st, "auto-login: " + api_err(r)); olog("auto-login failed: %s", api_err(r).c_str()); }
            });
        }
        bool visible = g_show.load();
        if (visible) {
            ImGui_ImplDX11_NewFrame();
            ImGui_ImplWin32_NewFrame();
            ImGui::NewFrame();
            ImGui::GetIO().MouseDrawCursor = true;   // the game hides the OS cursor; draw ImGui's own
            ClipCursor(nullptr);                     // the game clips the cursor to centre for camera look; free it
            draw_ui();
            ImGui::Render();
            g_ctx->OMSetRenderTargets(1, &g_rtv, nullptr);
            ImGui_ImplDX11_RenderDrawData(ImGui::GetDrawData());
        }
    }
    if (!g_scanning.load()) { g_scan.apply_frozen(); trainer_apply(); }
    return g_present_orig(sc, sync, flags);
}

static HRESULT STDMETHODCALLTYPE hk_resize(IDXGISwapChain *sc, UINT bc, UINT w, UINT h, DXGI_FORMAT fmt, UINT flags) {
    if (g_rtv) { g_rtv->Release(); g_rtv = nullptr; }
    HRESULT hr = g_resize_orig(sc, bc, w, h, fmt, flags);
    if (g_init) create_rtv(sc);
    return hr;
}

// Create a throwaway device + swap chain to read IDXGISwapChain's vtable, then hook Present/ResizeBuffers.
static DWORD WINAPI setup(LPVOID) {
    WNDCLASSEXW wc = {};
    wc.cbSize = sizeof(wc);
    wc.lpfnWndProc = DefWindowProcW;
    wc.hInstance = GetModuleHandleW(nullptr);
    wc.lpszClassName = L"PowderOverlayDummy";
    RegisterClassExW(&wc);
    HWND dummy = CreateWindowW(wc.lpszClassName, L"", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100,
                               nullptr, nullptr, wc.hInstance, nullptr);

    DXGI_SWAP_CHAIN_DESC scd = {};
    scd.BufferCount = 1;
    scd.BufferDesc.Width = 100;
    scd.BufferDesc.Height = 100;
    scd.BufferDesc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.OutputWindow = dummy;
    scd.SampleDesc.Count = 1;
    scd.Windowed = TRUE;
    scd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;

    IDXGISwapChain *sc = nullptr;
    ID3D11Device *dev = nullptr;
    ID3D11DeviceContext *ctx = nullptr;
    D3D_FEATURE_LEVEL fl;
    HRESULT hr = D3D11CreateDeviceAndSwapChain(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, 0, nullptr, 0,
                                               D3D11_SDK_VERSION, &scd, &sc, &dev, &fl, &ctx);
    if (FAILED(hr) || !sc) {
        olog("D3D11CreateDeviceAndSwapChain failed (hr=0x%08lx): cannot hook Present", (unsigned long)hr);
        if (dummy) DestroyWindow(dummy);
        return 0;
    }

    void **vtbl = *(void ***)sc;
    void *present_target = vtbl[8];       // IDXGISwapChain::Present
    void *resize_target = vtbl[13];       // IDXGISwapChain::ResizeBuffers
    sc->Release(); ctx->Release(); dev->Release();
    if (dummy) DestroyWindow(dummy);

    if (MH_Initialize() != MH_OK) { olog("MH_Initialize failed"); return 0; }
    if (MH_CreateHook(present_target, (void *)&hk_present, (void **)&g_present_orig) != MH_OK ||
        MH_CreateHook(resize_target, (void *)&hk_resize, (void **)&g_resize_orig) != MH_OK) {
        olog("MH_CreateHook failed");
        return 0;
    }
    MH_EnableHook(MH_ALL_HOOKS);
    olog("Present hooked (present=%p resize=%p); overlay armed, press HOME in-game", present_target, resize_target);
    return 0;
}

// ---- UI ---------------------------------------------------------------------------------------------------
static int g_sel = -1;                 // selected player index
static std::string g_sel_id;           // its profile id

static void enqueue_reload_selected() {
    if (g_sel_id.empty() || !g_net) return;
    std::string id = g_sel_id;
    g_net->enqueue([id](pw::HttpClient &c, AppState &st) { job_detail(c, st, id); });
}

static void draw_server_tab() {
    static char email[128] = "", pass[128] = "";
    bool logged_in; std::string who, status;
    { std::lock_guard<std::mutex> lk(g_st.m); logged_in = g_st.logged_in; who = g_st.who; status = g_st.status; }

    if (!logged_in) {
        ImGui::TextWrapped("Sign in with an account listed in [server] admins (the Rust server).");
        ImGui::InputText("Email", email, sizeof(email));
        ImGui::InputText("Password", pass, sizeof(pass), ImGuiInputTextFlags_Password);
        if (ImGui::Button("Sign in") && g_net) {
            std::string e = email, p = pass;
            g_net->enqueue([e, p](pw::HttpClient &c, AppState &st) {
                json b = {{"email", e}, {"password", p}};
                olog("sign in: POST /_powder/admin/api/login as '%s'", e.c_str());
                auto r = c.post("/_powder/admin/api/login", b.dump());
                if (r.ok()) {
                    std::string name;
                    try { auto j = json::parse(r.body); name = j.value("name", ""); std::lock_guard<std::mutex> lk(st.m);
                          st.logged_in = true; st.who = name; st.status = "signed in as " + name; } catch (...) {}
                    olog("sign in ok as %s", name.c_str());
                    job_players(c, st);
                } else push_log(st, "sign in: " + api_err(r));
            });
        }
        ImGui::TextDisabled("%s", status.c_str());
        return;
    }

    ImGui::Text("Signed in as %s", who.c_str());
    ImGui::SameLine();
    if (ImGui::SmallButton("Sign out") && g_net) {
        g_net->enqueue([](pw::HttpClient &c, AppState &st) {
            c.post("/_powder/admin/api/logout", "{}");
            c.clear_cookies();
            std::lock_guard<std::mutex> lk(st.m);
            st.logged_in = false; st.who.clear(); st.status = "signed out"; st.players.clear(); st.detail = Detail{};
        });
        g_sel = -1; g_sel_id.clear();
    }
    ImGui::TextDisabled("%s", status.c_str());
    ImGui::Separator();

    // broadcast
    static char bmsg[512] = "";
    static bool bchat = false;
    ImGui::SeparatorText("Message to everyone online");
    ImGui::InputText("##bmsg", bmsg, sizeof(bmsg));
    ImGui::SameLine();
    if (ImGui::Button("Send") && g_net && bmsg[0]) {
        std::string text = bmsg; bool chat = bchat;
        g_net->enqueue([text, chat](pw::HttpClient &c, AppState &st) {
            json b = {{"text", text}, {"chat", chat}};
            auto r = c.post("/_powder/admin/api/broadcast", b.dump());
            push_log(st, r.ok() ? "broadcast sent" : ("broadcast: " + api_err(r)));
        });
        bmsg[0] = 0;
    }
    ImGui::Checkbox("also as a Ubisoft Connect chat message", &bchat);

    // players
    ImGui::SeparatorText("Players");
    if (ImGui::Button("Refresh") && g_net)
        g_net->enqueue([](pw::HttpClient &c, AppState &st) { job_players(c, st); });

    std::vector<Player> players;
    { std::lock_guard<std::mutex> lk(g_st.m); players = g_st.players; }
    if (ImGui::BeginTable("players", 5, ImGuiTableFlags_Borders | ImGuiTableFlags_RowBg | ImGuiTableFlags_ScrollY,
                          ImVec2(0, 160))) {
        ImGui::TableSetupColumn("Name"); ImGui::TableSetupColumn("On");
        ImGui::TableSetupColumn("Lvl"); ImGui::TableSetupColumn("Credits"); ImGui::TableSetupColumn("Email");
        ImGui::TableHeadersRow();
        for (int i = 0; i < (int)players.size(); ++i) {
            ImGui::TableNextRow();
            ImGui::TableSetColumnIndex(0);
            bool selected = (g_sel == i);
            if (ImGui::Selectable((players[i].name + "##" + std::to_string(i)).c_str(), selected,
                                  ImGuiSelectableFlags_SpanAllColumns)) {
                g_sel = i; g_sel_id = players[i].profile_id; enqueue_reload_selected();
            }
            ImGui::TableSetColumnIndex(1); ImGui::TextUnformatted(players[i].online ? "*" : "");
            ImGui::TableSetColumnIndex(2); ImGui::TextUnformatted(players[i].level.c_str());
            ImGui::TableSetColumnIndex(3); ImGui::Text("%lld", players[i].credits);
            ImGui::TableSetColumnIndex(4); ImGui::TextUnformatted(players[i].email.c_str());
        }
        ImGui::EndTable();
    }

    // selected player detail
    if (g_sel >= 0 && g_sel < (int)players.size()) {
        Detail d;
        { std::lock_guard<std::mutex> lk(g_st.m); d = g_st.detail; }
        ImGui::SeparatorText(players[g_sel].name.c_str());
        if (d.loaded && d.profile_id == g_sel_id) {
            ImGui::Text("Credits: %lld", d.credits);

            static char gitem[64] = "credits";
            static int gamount = 1000;
            ImGui::InputText("item (or 'credits')", gitem, sizeof(gitem));
            ImGui::InputInt("amount", &gamount);
            if (ImGui::Button("Grant") && g_net) {
                std::string id = g_sel_id, item = gitem; int amt = gamount;
                g_net->enqueue([id, item, amt](pw::HttpClient &c, AppState &st) {
                    json b = {{"id", id}, {"item", item}, {"amount", amt}};
                    auto r = c.post("/_powder/admin/api/grant", b.dump());
                    push_log(st, r.ok() ? "granted" : ("grant: " + api_err(r)));
                    job_players(c, st); job_detail(c, st, id);
                });
            }

            if (!d.items.empty()) {
                ImGui::TextDisabled("Inventory:");
                for (auto &it : d.items) ImGui::BulletText("%s x%lld", it.item_id.c_str(), it.quantity);
            }

            ImGui::Spacing();
            ImGui::TextDisabled("Stats:");
            for (auto &s : d.stats)
                ImGui::BulletText("%s = %s", s.key.c_str(), s.is_double ? std::to_string(s.value).c_str()
                                                                        : std::to_string((long long)s.value).c_str());
            static char skey[64] = "", sval[64] = "";
            static bool sdouble = false;
            ImGui::InputText("stat key", skey, sizeof(skey));
            ImGui::InputText("value", sval, sizeof(sval));
            ImGui::SameLine(); ImGui::Checkbox("float", &sdouble);
            if (ImGui::Button("Set stat") && g_net && skey[0]) {
                std::string id = g_sel_id, key = skey, val = sval; bool dbl = sdouble;
                g_net->enqueue([id, key, val, dbl](pw::HttpClient &c, AppState &st) {
                    json b = {{"id", id}, {"key", key}, {"double", dbl}};
                    try { b["value"] = std::stod(val); } catch (...) { b["value"] = 0; }
                    auto r = c.post("/_powder/admin/api/stat", b.dump());
                    push_log(st, r.ok() ? "stat set" : ("stat: " + api_err(r)));
                    job_detail(c, st, id);
                });
            }

            ImGui::Spacing();
            if (ImGui::Button("Reset player...")) ImGui::OpenPopup("confirm_reset");
            if (ImGui::BeginPopup("confirm_reset")) {
                ImGui::Text("Wipe %s? They start again as a new player.", players[g_sel].name.c_str());
                if (ImGui::Button("Yes, reset") && g_net) {
                    std::string id = g_sel_id;
                    g_net->enqueue([id](pw::HttpClient &c, AppState &st) {
                        json b = {{"id", id}};
                        auto r = c.post("/_powder/admin/api/reset", b.dump());
                        push_log(st, r.ok() ? "player reset" : ("reset: " + api_err(r)));
                        job_players(c, st); job_detail(c, st, id);
                    });
                    ImGui::CloseCurrentPopup();
                }
                ImGui::SameLine();
                if (ImGui::Button("Cancel")) ImGui::CloseCurrentPopup();
                ImGui::EndPopup();
            }
        } else {
            ImGui::TextDisabled("loading...");
        }
    }
}

static void draw_debug_tab() {
    ImGui::Text("Server: %s", g_net ? g_net->host().c_str() : "(none)");
    {
        std::lock_guard<std::mutex> lk(g_st.m);
        ImGui::Text("Signed in: %s", g_st.logged_in ? g_st.who.c_str() : "no");
        ImGui::Text("Last: %s", g_st.status.c_str());
    }
    ImGui::Text("Overlay: %.1f FPS", ImGui::GetIO().Framerate);
    if (ImGui::Button("Poll feed") && g_net)
        g_net->enqueue([](pw::HttpClient &c, AppState &st) { job_feed(c, st); });

    ImGui::SeparatorText("Broadcast feed");
    {
        std::lock_guard<std::mutex> lk(g_st.m);
        ImGui::BeginChild("feed", ImVec2(0, 80), ImGuiChildFlags_Border);
        for (auto &f : g_st.feed) ImGui::TextWrapped("%s", f.c_str());
        ImGui::EndChild();
    }

    ImGui::SeparatorText("HTTP activity");
    {
        std::lock_guard<std::mutex> lk(g_st.m);
        ImGui::BeginChild("httplog", ImVec2(0, 80), ImGuiChildFlags_Border);
        for (auto it = g_st.httplog.rbegin(); it != g_st.httplog.rend(); ++it) ImGui::TextWrapped("%s", it->c_str());
        ImGui::EndChild();
    }

    // ---- memory scanner ----
    ImGui::SeparatorText("Memory scanner");
    static int type_idx = 0;                 // 0=int32 1=int64 2=float 3=double
    static char sval[64] = "";
    const char *types[] = {"int32", "int64", "float", "double"};
    ImGui::Combo("type", &type_idx, types, 4);
    ImGui::InputText("value##scan", sval, sizeof(sval));
    pw::ValType vt = (pw::ValType)type_idx;

    bool scanning = g_scanning.load();
    ImGui::BeginDisabled(scanning);
    if (ImGui::Button("First scan")) {
        pw::Value v;
        if (pw::Value::parse(vt, sval, v)) {
            g_scanning = true;
            std::thread([v] { g_scan.first_scan(v); g_scanning = false; }).detach();
        }
    }
    ImGui::SameLine();
    if (ImGui::Button("First scan (unknown)")) {   // no value known: snapshot, then narrow with the buttons below
        g_scanning = true;
        std::thread([vt] { g_scan.first_scan_unknown(vt); g_scanning = false; }).detach();
    }
    ImGui::SameLine();
    if (ImGui::Button("Reset##scan")) g_scan.reset();
    ImGui::TextDisabled("For coords: type float, First scan (unknown), move -> changed, stand still -> unchanged, repeat.");
    // next-scan comparisons
    auto next = [&](pw::Compare cmp) {
        pw::Value v; pw::Value::parse(vt, sval, v);
        g_scanning = true;
        std::thread([v, cmp] { g_scan.next_scan(cmp, v); g_scanning = false; }).detach();
    };
    if (ImGui::Button("= value")) next(pw::Compare::Exact);
    ImGui::SameLine();
    if (ImGui::Button("changed")) next(pw::Compare::Changed);
    ImGui::SameLine();
    if (ImGui::Button("unchanged")) next(pw::Compare::Unchanged);
    ImGui::SameLine();
    if (ImGui::Button(">")) next(pw::Compare::Greater);
    ImGui::SameLine();
    if (ImGui::Button("<")) next(pw::Compare::Less);
    ImGui::EndDisabled();

    if (scanning) ImGui::TextDisabled("scanning...");
    else {
        size_t n = g_scan.count();
        ImGui::Text("%zu match(es)%s", n, n > 1000 ? " (showing first 1000)" : "");
        if (ImGui::BeginTable("matches", 3, ImGuiTableFlags_Borders | ImGuiTableFlags_ScrollY, ImVec2(0, 140))) {
            ImGui::TableSetupColumn("Address"); ImGui::TableSetupColumn("Value"); ImGui::TableSetupColumn("");
            ImGui::TableHeadersRow();
            size_t show = n < 1000 ? n : 1000;
            for (size_t i = 0; i < show; ++i) {
                ImGui::TableNextRow();
                ImGui::TableSetColumnIndex(0); ImGui::Text("%p", (void *)g_scan.addr(i));
                pw::Value cur;
                ImGui::TableSetColumnIndex(1);
                if (g_scan.read(i, cur)) ImGui::TextUnformatted(cur.str().c_str()); else ImGui::TextDisabled("?");
                ImGui::TableSetColumnIndex(2);
                if (ImGui::SmallButton((std::string("freeze##") + std::to_string(i)).c_str())) {
                    pw::Value v;
                    if (pw::Value::parse(vt, sval, v)) g_scan.frozen.push_back({g_scan.addr(i), v});
                }
                ImGui::SameLine();
                if (ImGui::SmallButton((std::string("-> Trainer##") + std::to_string(i)).c_str()))
                    add_slot("value", g_scan.addr(i), vt);
            }
            ImGui::EndTable();
        }
        if (!g_scan.frozen.empty()) {
            ImGui::Text("Frozen:");
            for (size_t i = 0; i < g_scan.frozen.size(); ++i) {
                ImGui::BulletText("%p = %s", (void *)g_scan.frozen[i].addr, g_scan.frozen[i].value.str().c_str());
                ImGui::SameLine();
                if (ImGui::SmallButton((std::string("x##fr") + std::to_string(i)).c_str())) {
                    g_scan.frozen.erase(g_scan.frozen.begin() + i); break;
                }
            }
        }
    }
}

static const char *TYPE_NAMES[] = {"int32", "int64", "float", "double"};

static void draw_trainer_tab() {
    ImGui::SeparatorText("Player");
    if (!pw::trainer_loaded()) {
        ImGui::TextDisabled("no powder_trainer.json beside the overlay (powder-wrap deploys it)");
    } else {
        if (ImGui::Button("Find player")) track_player();
        ImGui::SameLine();
        if (g_player_pos) ImGui::Text("position at %p (%s)", (void *)g_player_pos, g_player_why.c_str());
        else ImGui::TextDisabled("%s", g_player_why.c_str());
    }

    ImGui::SeparatorText("Slots");
    ImGui::TextWrapped("Addresses you found (Debug tab's scanner -> \"-> Trainer\") or the player's. Freeze a value, "
                       "or set three slots as X/Y/Z and enable Fly.");

    // manual add
    static char maddr[32] = "";
    static int mtype = 2;   // default float (positions are floats)
    ImGui::SetNextItemWidth(160); ImGui::InputText("addr (hex)##add", maddr, sizeof(maddr));
    ImGui::SameLine(); ImGui::SetNextItemWidth(90); ImGui::Combo("##mtype", &mtype, TYPE_NAMES, 4);
    ImGui::SameLine();
    if (ImGui::Button("Add slot")) {
        uintptr_t a = (uintptr_t)strtoull(maddr, nullptr, 16);
        if (a) add_slot("slot", a, (pw::ValType)mtype);
    }

    if (g_slots.empty()) { ImGui::TextDisabled("no slots yet"); return; }

    if (ImGui::BeginTable("slots", 6, ImGuiTableFlags_Borders | ImGuiTableFlags_RowBg)) {
        ImGui::TableSetupColumn("Label"); ImGui::TableSetupColumn("Address"); ImGui::TableSetupColumn("Value");
        ImGui::TableSetupColumn("Freeze"); ImGui::TableSetupColumn("Set"); ImGui::TableSetupColumn("");
        ImGui::TableHeadersRow();
        int remove = -1;
        for (int i = 0; i < (int)g_slots.size(); ++i) {
            Slot &s = g_slots[i];
            ImGui::PushID(i);
            ImGui::TableNextRow();
            ImGui::TableSetColumnIndex(0);
            ImGui::SetNextItemWidth(90); ImGui::InputText("##lbl", s.label, sizeof(s.label));
            ImGui::TableSetColumnIndex(1);
            ImGui::Text("%p", (void *)s.addr);
            const char *ax = g_axis[0] == i ? " [X]" : g_axis[1] == i ? " [Y]" : g_axis[2] == i ? " [Z]" : "";
            if (ax[0]) { ImGui::SameLine(); ImGui::TextDisabled("%s", ax); }
            ImGui::TableSetColumnIndex(2);
            pw::Value cur;
            if (pw::peek(s.addr, s.type, cur)) ImGui::TextUnformatted(cur.str().c_str()); else ImGui::TextDisabled("?");
            ImGui::TableSetColumnIndex(3);
            if (ImGui::Checkbox("##fz", &s.freeze) && s.freeze) pw::peek(s.addr, s.type, s.frozen);
            ImGui::TableSetColumnIndex(4);
            ImGui::SetNextItemWidth(70);
            ImGui::InputText("##set", s.input, sizeof(s.input));
            ImGui::SameLine();
            if (ImGui::SmallButton("set")) {
                pw::Value v;
                if (s.input[0] && pw::Value::parse(s.type, s.input, v)) { pw::poke(s.addr, v); s.frozen = v; }
            }
            ImGui::TableSetColumnIndex(5);
            if (ImGui::SmallButton("X")) g_axis[0] = i;
            ImGui::SameLine(); if (ImGui::SmallButton("Y")) g_axis[1] = i;
            ImGui::SameLine(); if (ImGui::SmallButton("Z")) g_axis[2] = i;
            ImGui::SameLine(); if (ImGui::SmallButton("del")) remove = i;
            ImGui::PopID();
        }
        ImGui::EndTable();
        if (remove >= 0) {
            if (g_fly) set_fly(false);
            erase_slot(remove);
        }
    }

    ImGui::SeparatorText("Fly / noclip");
    bool axes_ok = g_axis[0] >= 0 && g_axis[1] >= 0 && g_axis[2] >= 0;
    ImGui::BeginDisabled(!axes_ok);
    bool fly = g_fly;
    if (ImGui::Checkbox("Fly (F6; W/A/S/D, Shift = up, Ctrl = down)", &fly)) set_fly(fly);
    ImGui::EndDisabled();
    if (!axes_ok) ImGui::TextDisabled("find the player, or assign X, Y and Z (the buttons in each row)");
    ImGui::BeginDisabled(!pw::trainer_loaded());
    if (ImGui::Checkbox("Noclip while flying (the game can't move or block you)", &g_noclip_with_fly) && g_fly)
        set_fly(true);
    ImGui::EndDisabled();
    if (!g_noclip_err.empty()) ImGui::TextColored(ImVec4(1, 0.5f, 0.4f, 1), "noclip: %s", g_noclip_err.c_str());
    else if (pw::noclip_active()) ImGui::TextDisabled("noclip on: physics and collision are off for you");
    ImGui::SetNextItemWidth(120); ImGui::InputFloat("speed (units/frame)", &g_fly_speed);

    ImGui::SeparatorText("Teleport");
    ImGui::BeginDisabled(!axes_ok);
    if (ImGui::Button("Save spot")) {
        g_have_spot = pw::peek(g_slots[g_axis[0]].addr, g_slots[g_axis[0]].type, g_spot[0]) &&
                      pw::peek(g_slots[g_axis[1]].addr, g_slots[g_axis[1]].type, g_spot[1]) &&
                      pw::peek(g_slots[g_axis[2]].addr, g_slots[g_axis[2]].type, g_spot[2]);
    }
    ImGui::SameLine();
    ImGui::BeginDisabled(!g_have_spot);
    if (ImGui::Button("Teleport to spot"))
        for (int k = 0; k < 3; ++k) pw::poke(g_slots[g_axis[k]].addr, g_spot[k]);
    ImGui::EndDisabled();
    ImGui::EndDisabled();

    // ---- access-finder: what code writes a slot (the bridge to real hooks / a stable pointer) ----
    ImGui::SeparatorText("Find what writes an address (RE)");
    ImGui::TextColored(ImVec4(1, 0.7f, 0.3f, 1), "Crashes the game on hot data like the position: use "
                       "tools/trainer/watch.py (outside the game) for that.");
    static char afaddr[32] = "";
    ImGui::SetNextItemWidth(160);
    ImGui::InputText("addr (hex)##af", afaddr, sizeof(afaddr));
    ImGui::SameLine();
    if (!pw::g_access.armed()) {
        if (ImGui::Button("Watch")) {
            uint64_t a = (uint64_t)strtoull(afaddr, nullptr, 16);
            if (!a && !g_slots.empty() && g_axis[0] >= 0) a = g_slots[g_axis[0]].addr;   // default: the X slot
            if (a) pw::g_access.arm(a, 4);
        }
        ImGui::SameLine();
        ImGui::TextDisabled("(blank = the X slot) then move/act in-game");
    } else {
        if (ImGui::Button("Stop")) pw::g_access.disarm();
        ImGui::SameLine(); ImGui::Text("watching 0x%llx", (unsigned long long)pw::g_access.target());
    }
    ImGui::TextDisabled("%s", pw::g_access.status());

    auto hits = pw::g_access.hits();
    if (!hits.empty()) {
        ImGui::Text("%zu instruction(s) touch it:", hits.size());
        if (ImGui::BeginTable("afhits", 5, ImGuiTableFlags_Borders | ImGuiTableFlags_ScrollY, ImVec2(0, 150))) {
            ImGui::TableSetupColumn("Code"); ImGui::TableSetupColumn("RW"); ImGui::TableSetupColumn("Operand");
            ImGui::TableSetupColumn("Struct base + off"); ImGui::TableSetupColumn("");
            ImGui::TableHeadersRow();
            for (size_t i = 0; i < hits.size(); ++i) {
                const pw::AccessHit &h = hits[i];
                ImGui::PushID((int)i);
                ImGui::TableNextRow();
                ImGui::TableSetColumnIndex(0); ImGui::Text("%s+0x%llx", h.module.c_str(), (unsigned long long)h.module_off);
                ImGui::TableSetColumnIndex(1); ImGui::TextUnformatted(h.write ? "W" : "r");
                ImGui::TableSetColumnIndex(2); ImGui::Text("%s x%llu", h.text.c_str(), (unsigned long long)h.count);
                ImGui::TableSetColumnIndex(3);
                if (h.base_reg >= 0) ImGui::Text("0x%llx + 0x%lx", (unsigned long long)h.base_val, h.disp);
                else ImGui::TextDisabled("rip-rel");
                ImGui::TableSetColumnIndex(4);
                if (h.write) {
                    if (h.nopped) ImGui::TextDisabled("NOP'd");
                    else if (ImGui::SmallButton("NOP")) pw::g_access.nop(h.rip);
                }
                ImGui::PopID();
            }
            ImGui::EndTable();
        }
        if (ImGui::SmallButton("clear hits")) pw::g_access.clear();
        ImGui::SameLine();
        if (ImGui::SmallButton("restore NOPs")) pw::g_access.restore_all();
        ImGui::SameLine();
        ImGui::TextDisabled("NOP a writer to freeze/override that field");
    }
}

static void draw_ui() {
    ImGui::SetNextWindowSize(ImVec2(520, 580), ImGuiCond_FirstUseEver);
    ImGui::Begin("Powder");
    if (g_st.busy.load()) { ImGui::SameLine(ImGui::GetWindowWidth() - 60); ImGui::TextDisabled("..."); }
    if (ImGui::BeginTabBar("tabs")) {
        if (ImGui::BeginTabItem("Server")) { draw_server_tab(); ImGui::EndTabItem(); }
        if (ImGui::BeginTabItem("Debug")) { draw_debug_tab(); ImGui::EndTabItem(); }
        if (ImGui::BeginTabItem("Trainer")) { draw_trainer_tab(); ImGui::EndTabItem(); }
        if (ImGui::BeginTabItem("Mods")) { pw::mods_draw_tab(); ImGui::EndTabItem(); }
        ImGui::EndTabBar();
    }
    ImGui::End();
}

// ---- entry -----------------------------------------------------------------------------------------------
BOOL WINAPI DllMain(HINSTANCE h, DWORD reason, LPVOID) {
    if (reason != DLL_PROCESS_ATTACH) return TRUE;
    DisableThreadLibraryCalls(h);
    wchar_t self[MAX_PATH];
    GetModuleFileNameW(h, self, MAX_PATH);
    wcscpy(g_dir, self);
    wchar_t *slash = wcsrchr(g_dir, L'\\');
    if (slash) slash[1] = 0; else g_dir[0] = 0;
    _snwprintf(g_logpath, MAX_PATH, L"%spowder_overlay.log", g_dir);
    _snwprintf(g_cfgpath, MAX_PATH, L"%spowder_overlay.cfg", g_dir);
    read_cfg();
    wchar_t trainer[MAX_PATH];
    _snwprintf(trainer, MAX_PATH, L"%spowder_trainer.json", g_dir);
    std::string terr;
    wchar_t armer[MAX_PATH];
    _snwprintf(armer, MAX_PATH, L"%sarmer.exe", g_dir);
    pw::code_set_helper(armer);
    if (pw::trainer_load(trainer, terr)) olog("trainer data loaded (powder_trainer.json)");
    else olog("trainer: %s", terr.c_str());
    olog("overlay DLL loaded pid=%lu, server=%s%s", GetCurrentProcessId(), g_base_url.c_str(),
         g_cfg_show ? " [force-show]" : "");
    g_net = new Net(g_base_url);
    if (!g_net->valid()) olog("WARNING: could not parse server url '%s'", g_base_url.c_str());
    g_net->start();
    CreateThread(nullptr, 0, setup, nullptr, 0, nullptr);
    return TRUE;
}
