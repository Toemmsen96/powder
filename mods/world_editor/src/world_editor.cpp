// world_editor - the overlay's World editor panel, the client half of mods/world_editor (README.md there): where
// the player is (position, map cell), the objects near them (the map's index, which apply.py builds from the
// player's own install at game start and powder-wrap puts beside this DLL), adding copies of placeable props,
// moving / turning / scaling / removing objects, and the list of edits with undo. Edits go to the server
// (mod.lua, /_powder/world_editor/), which keeps them. World cells are loaded from archives: an edit shows at the
// next game start (the session step writes the edited cells into a _patch_03), and with the session helper running
// (live.py) as soon as the game reads its cell again (live.cpp serves the edited cell in place of the archive's
// bytes; only an edit that makes a cell bigger points the game's index record of that cell elsewhere).
//
// "In front" is the direction the rider last moved in (the camera is not part of the mod API).
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <string>
#include <thread>
#include <unordered_map>
#include <vector>
#include "imgui.h"
#include "json.hpp"
#include "powder_api.h"
#include "live.h"

using json = nlohmann::json;
constexpr double PI = 3.14159265358979323846;

namespace {

const PowderApi *g_api;
PowderMod *g_mod;
std::wstring g_dir;                                    // <game>\powder_mods\world_editor\ (the index files)
const char *const MAPS[] = {"Alpes", "Japan", "Alaska", "WhiteRoom"};
constexpr int NMAPS = 4;
constexpr const char *BASE = "/_powder/world_editor/";
// Placeable props listed first (substrings of the prop name, any case): trees, rocks, kickers and ramps, things
// to sit on, round objects to ride into.
const char *const FAVOURITES[] = {"Sapin", "ROCK_", "Kick", "QuarterPipe", "BigAir", "Ramp", "Platform", "Bench",
                                  "Snowman", "Barrel", "Rabbid", "Table", "WoodPile", "TrunkPile", "Haycart",
                                  "Parasol", "Transat", "Balloon", "Baloon"};

// ---- the map index (world.py index) ------------------------------------------------------------------------
struct Obj {
    int layer = 0, cell = 0;
    uint64_t id = 0;
    bool group = false;
    float pos[3] = {0, 0, 0}, yaw = 0, scale = 1, size[3] = {0, 0, 0};
    std::string name, prop;
};
struct Prop { std::string name; int count = 0; size_t example = 0; bool fav = false; float size[3] = {0, 0, 0}; };
struct Grid {
    double min_x = 0, min_y = 0, cell = 128;
    int levels = 1;
    int side(int level) const { return 1 << (levels - 1 - level); }
    int first(int level) const { int n = 0; for (int k = 0; k < level; ++k) n += side(k) * side(k); return n; }
    int at(double x, double y, int level) const {
        double size = cell * (1 << level);
        int col = (int)std::floor((x - min_x) / size), row = (int)std::floor((y - min_y) / size), s = side(level);
        return col >= 0 && col < s && row >= 0 && row < s ? first(level) + row * s + col : -1;
    }
    // index -> level, square (x0, y0, size)
    bool rect(int index, int &level, double &x0, double &y0, double &size) const {
        for (level = 0; level < levels; ++level) {
            int s = side(level);
            if (index < s * s) {
                size = cell * (1 << level);
                x0 = min_x + (index % s) * size;
                y0 = min_y + (index / s) * size;
                return true;
            }
            index -= s * s;
        }
        return false;
    }
};
struct Index {
    std::string map, error;
    Grid grid;
    bool has_grid = false;
    std::vector<Obj> objs;
    std::vector<Prop> props;
    std::set<std::pair<int, int>> cells;
};

std::string lower(std::string s) {
    for (auto &c : s) c = (char)tolower((unsigned char)c);
    return s;
}

std::shared_ptr<Index> load_index(const std::string &map) {
    auto ix = std::make_shared<Index>();
    ix->map = map;
    std::wstring path = g_dir + std::wstring(map.begin(), map.end()) + L".tsv";
    FILE *f = _wfopen(path.c_str(), L"rb");
    if (!f) {
        ix->error = "no index for " + map + " (it is built at game start: POWDER_CLIENT_MODS=world_editor with "
                    "the overlay)";
        return ix;
    }
    std::string data;
    char buf[1 << 16];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) data.append(buf, n);
    fclose(f);
    std::vector<std::string> col;
    size_t at = 0;
    while (at < data.size()) {
        size_t end = data.find('\n', at);
        if (end == std::string::npos) end = data.size();
        std::string line = data.substr(at, end - at);
        at = end + 1;
        if (line.empty() || line[0] == '#') continue;
        col.clear();
        size_t s = 0;
        for (;;) {
            size_t t = line.find('\t', s);
            col.push_back(line.substr(s, t == std::string::npos ? std::string::npos : t - s));
            if (t == std::string::npos) break;
            s = t + 1;
        }
        if (col[0] == "obj" && col.size() >= 15) {
            Obj o;
            o.layer = atoi(col[1].c_str());
            o.cell = atoi(col[2].c_str());
            o.id = strtoull(col[3].c_str(), nullptr, 16);
            o.group = col[4] == "group";
            for (int k = 0; k < 3; ++k) o.pos[k] = strtof(col[5 + k].c_str(), nullptr);
            o.yaw = strtof(col[8].c_str(), nullptr);
            o.scale = strtof(col[9].c_str(), nullptr);
            for (int k = 0; k < 3; ++k) o.size[k] = strtof(col[10 + k].c_str(), nullptr);
            o.name = col[13];
            o.prop = col[14];
            ix->objs.push_back(std::move(o));
        } else if (col[0] == "grid" && col.size() >= 6 && col[1] == "0") {
            ix->grid.min_x = strtod(col[2].c_str(), nullptr);
            ix->grid.min_y = strtod(col[3].c_str(), nullptr);
            ix->grid.cell = strtod(col[4].c_str(), nullptr);
            ix->grid.levels = std::max(1, std::min(12, atoi(col[5].c_str())));
            ix->has_grid = true;
        } else if (col[0] == "cell" && col.size() >= 3) {
            ix->cells.insert({atoi(col[1].c_str()), atoi(col[2].c_str())});
        }
    }
    // placeable props: entities with a prop template (world.py copies entities only); one example each, from the
    // finest level (small cells stream with what is around them)
    std::unordered_map<std::string, size_t> by_prop;
    int level0 = ix->grid.side(0) * ix->grid.side(0);
    for (size_t i = 0; i < ix->objs.size(); ++i) {
        const Obj &o = ix->objs[i];
        if (o.group || o.prop.empty()) continue;
        auto it = by_prop.find(o.prop);
        if (it == by_prop.end()) {
            Prop p;
            p.name = o.prop;
            p.example = i;
            std::string l = lower(o.prop);
            for (const char *fav : FAVOURITES) p.fav |= l.find(lower(fav)) != std::string::npos;
            by_prop[o.prop] = ix->props.size();
            ix->props.push_back(p);
            it = by_prop.find(o.prop);
        }
        Prop &p = ix->props[it->second];
        p.count++;
        const Obj &ex = ix->objs[p.example];
        if (ex.cell >= level0 && o.cell < level0) p.example = i;
    }
    for (auto &p : ix->props) memcpy(p.size, ix->objs[p.example].size, sizeof p.size);
    std::sort(ix->props.begin(), ix->props.end(), [](const Prop &a, const Prop &b) {
        return a.fav != b.fav ? a.fav : a.count != b.count ? a.count > b.count : a.name < b.name;
    });
    if (!ix->has_grid) ix->error = "the index has no grid line";
    return ix;
}

// ---- shared state (render thread, the mod host's network thread, the index loader) ---------------------------
std::mutex g_m;
std::shared_ptr<Index> g_loaded;                       // the last index loaded (any map)
bool g_loading = false;
std::vector<json> g_net_edits;                         // the server's list, as last received
int g_net_ver = 0;
bool g_busy = false;
std::string g_status = "asking the server for the edits...";
float g_pos[3] = {0, 0, 0};
bool g_have_pos = false;
float g_heading = 0;                                   // radians from +X, counter-clockwise (the rider's motion)
bool g_have_heading = false;

void on_reply(int status, const char *body, size_t len, void *user) {
    const char *what = (const char *)user;
    json j = json::parse(body ? std::string(body, len) : std::string(), nullptr, false);
    std::lock_guard<std::mutex> lk(g_m);
    g_busy = false;
    if (status == 200 && j.is_object() && j.contains("edits") && j["edits"].is_array()) {
        g_net_edits.assign(j["edits"].begin(), j["edits"].end());
        g_net_ver++;
        g_status = std::string(what) + ": ok (" + std::to_string(g_net_edits.size()) + " edit(s) stored)";
        if (strcmp(what, "load") != 0) live::poke();   // the live helper asks for the list now
    } else if (status == 0) {
        g_status = std::string(what) + ": no reply from the server";
    } else {
        std::string err = j.is_object() && j.contains("error") && j["error"].is_string() ? j["error"].get<std::string>()
                          : status == 404 ? "not found (the server runs no mods/world_editor: needs the Rust server)"
                                          : "HTTP " + std::to_string(status);
        g_status = std::string(what) + ": " + err;
    }
}

void request(const char *method, const char *action, const json &body, const char *what) {
    std::string path = std::string(BASE) + action;
    std::string text = body.is_null() ? std::string() : body.dump();
    {
        std::lock_guard<std::mutex> lk(g_m);
        g_busy = true;
        g_status = std::string(what) + "...";
    }
    if (g_api->http(g_mod, method, path.c_str(), text.empty() ? nullptr : text.c_str(), on_reply, (void *)what)) {
        std::lock_guard<std::mutex> lk(g_m);
        g_busy = false;
        g_status = std::string(what) + ": " + g_api->last_error();
    }
}

// ---- per frame: the player's position and heading -------------------------------------------------------------
uintptr_t g_pos_addr = 0;
DWORD g_pos_checked = 0;
float g_anchor[2] = {0, 0};
bool g_anchored = false;

void frame(void *) {
    DWORD now = GetTickCount();
    if (now - g_pos_checked > 1000) {                    // the position moves on a map change or a respawn
        g_pos_checked = now;
        g_pos_addr = g_api->player_position();
    }
    float p[3];
    bool ok = g_pos_addr && g_api->read(g_pos_addr, p, sizeof p) == 0 && std::isfinite(p[0]) && std::isfinite(p[1]) &&
              std::isfinite(p[2]) && std::fabs(p[0]) < 1e5f && std::fabs(p[1]) < 1e5f;
    std::lock_guard<std::mutex> lk(g_m);
    g_have_pos = ok;
    if (!ok) return;
    memcpy(g_pos, p, sizeof p);
    float dx = p[0] - g_anchor[0], dy = p[1] - g_anchor[1], d = std::sqrt(dx * dx + dy * dy);
    if (!g_anchored || d > 60) {                         // first sight, or a teleport
        g_anchor[0] = p[0]; g_anchor[1] = p[1]; g_anchored = true;
    } else if (d > 1.5f) {
        g_heading = std::atan2(dy, dx);
        g_have_heading = true;
        g_anchor[0] = p[0]; g_anchor[1] = p[1];
    }
}

// ---- the edit list folded into what the world will look like (as world.py apply folds it) --------------------
struct Change { bool moved = false; float pos[3] = {0, 0, 0}; float yaw = 0, scale = 1; bool removed = false; };
struct Added {
    int n = 0;
    std::string name, src_id;
    int src_layer = 0, src_cell = 0;
    float pos[3] = {0, 0, 0}, yaw = 0, scale = 1;
};

std::string hex(uint64_t v) {
    char b[24];
    snprintf(b, sizeof b, "0x%llx", (unsigned long long)v);
    return b;
}
// One shipped object of a map (ids are < 2^42, cells < 2^17)
uint64_t obj_key(int layer, int cell, uint64_t id) {
    return (uint64_t)(cell & 0x1FFFF) << 44 | (uint64_t)(layer & 3) << 42 | (id & ((1ull << 42) - 1));
}
double num(const json &j, const char *k, double dflt) {
    return j.contains(k) && j[k].is_number() ? j[k].get<double>() : dflt;
}
bool pos_of(const json &j, float out[3]) {
    if (!j.contains("pos") || !j["pos"].is_array() || j["pos"].size() < 3) return false;
    for (int k = 0; k < 3; ++k) {
        if (!j["pos"][k].is_number()) return false;
        out[k] = j["pos"][k].get<float>();
    }
    return true;
}

struct World {
    std::unordered_map<uint64_t, Change> changes;      // obj_key -> change
    std::map<int, Added> added;                        // edit n -> object
};

World fold(const std::vector<json> &edits, const std::string &map) {
    World w;
    for (const json &e : edits) {
        if (!e.is_object() || e.value("map", "") != map) continue;
        std::string op = e.value("op", "");
        int n = e.value("n", 0);
        if (op == "add" && e.contains("src") && e["src"].is_object()) {
            Added a;
            a.n = n;
            a.name = e.value("name", "");
            a.src_layer = e["src"].value("layer", 0);
            a.src_cell = e["src"].value("cell", 0);
            a.src_id = e["src"].value("id", "");
            pos_of(e, a.pos);
            a.yaw = (float)num(e, "yaw", 0);
            a.scale = (float)num(e, "scale", 1);
            w.added[n] = a;
            continue;
        }
        float *pos, *yaw, *scale;
        Change *ch = nullptr;
        Added *ad = nullptr;
        if (e.contains("of") && e["of"].is_number_integer()) {
            auto it = w.added.find(e["of"].get<int>());
            if (it == w.added.end()) continue;
            ad = &it->second;
            pos = ad->pos; yaw = &ad->yaw; scale = &ad->scale;
        } else if (e.contains("obj") && e["obj"].is_object()) {
            const json &o = e["obj"];
            std::string id = o.value("id", "");
            ch = &w.changes[obj_key(o.value("layer", 0), o.value("cell", 0), strtoull(id.c_str(), nullptr, 16))];
            if (ch->removed) continue;
            pos = ch->pos; yaw = &ch->yaw; scale = &ch->scale;
        } else {
            continue;
        }
        if (op == "move" && pos_of(e, pos)) { if (ch) ch->moved = true; }
        else if (op == "turn") *yaw += (float)num(e, "deg", 0);
        else if (op == "scale") *scale *= (float)num(e, "factor", 1);
        else if (op == "remove") {
            if (ch) ch->removed = true;
            else { int k = ad->n; w.added.erase(k); }
        }
    }
    return w;
}

// ---- the panel ------------------------------------------------------------------------------------------------
struct Near {
    bool added = false;
    int n = 0;                                         // added: its edit
    const Obj *obj = nullptr;                          // shipped
    float pos[3] = {0, 0, 0}, yaw = 0, scale = 1, dist = 0;
    bool changed = false;
    std::string name;
};

int g_map = 0, g_map_loaded = -1;
bool g_map_chosen = false;                             // the player picked one (else: the newest edit's map)
std::shared_ptr<Index> g_ix;
std::vector<json> g_edits;
int g_edits_ver = -1;
World g_world;
bool g_world_dirty = true;
bool g_asked = false;
char g_filter[64] = "";
bool g_favs_only = false;
int g_prop = -1;                                       // index into g_ix->props
float g_ahead = 6, g_dz = 0, g_add_yaw = 0, g_add_scale = 1;
float g_radius = 40;
bool g_all_kinds = false;                              // also objects without a prop (cameras, anchors, FX)
std::string g_sel;                                     // "a:<n>" or "o:<layer>:<cell>:<id>"
float g_sel_pos[3] = {0, 0, 0};
float g_turn = 15, g_factor = 1.25f;

double r3(double v) { return std::round(v * 1000.0) / 1000.0; }   // JSON without float noise
json jpos(const float p[3]) { return json::array({r3(p[0]), r3(p[1]), r3(p[2])}); }

void ahead_of(const float me[3], float out[3], float dist, bool heading_known, float heading) {
    float h = heading_known ? heading : (float)(PI / 2);   // unknown: +Y (north)
    out[0] = me[0] + std::cos(h) * dist;
    out[1] = me[1] + std::sin(h) * dist;
    out[2] = me[2] + g_dz;
}

// The finest cell of the main table at (x, y) that exists (where world.py apply puts an added object), or -1.
int target_cell(const Index &ix, double x, double y, int &level) {
    for (level = 0; level < ix.grid.levels; ++level) {
        int i = ix.grid.at(x, y, level);
        if (i >= 0 && ix.cells.count({0, i})) return i;
    }
    return -1;
}

json target_of(const Near &s) {
    json j = {{"map", MAPS[g_map]}};
    if (s.added) j["of"] = s.n;
    else j["obj"] = {{"layer", s.obj->layer}, {"cell", s.obj->cell}, {"id", hex(s.obj->id)}, {"name", s.obj->name}};
    return j;
}

void draw_position(const float me[3], bool have_pos, bool have_heading, float heading) {
    if (!have_pos) {
        ImGui::TextDisabled("player position unknown (the overlay's Trainer tab finds it once you ride)");
        return;
    }
    ImGui::Text("position  x %.1f  y %.1f  z %.1f", me[0], me[1], me[2]);
    if (have_heading) {
        ImGui::SameLine();
        ImGui::TextDisabled("  heading %.0f deg", heading * 180.0 / PI);
    }
    if (!g_ix || !g_ix->has_grid) return;
    const Grid &g = g_ix->grid;
    int c0 = g.at(me[0], me[1], 0), level;
    int t = target_cell(*g_ix, me[0], me[1], level);
    if (c0 < 0) {
        ImGui::TextDisabled("outside %s's cells", MAPS[g_map]);
    } else {
        int lvl = 0; double x0 = 0, y0 = 0, size = 0;
        g.rect(c0, lvl, x0, y0, size);
        ImGui::Text("cell %05d (x %.0f..%.0f, y %.0f..%.0f)%s", c0, x0, x0 + size, y0, y0 + size,
                    g_ix->cells.count({0, c0}) ? "" : ": empty");
        if (t >= 0 && t != c0) {
            ImGui::SameLine();
            ImGui::TextDisabled(" - added objects go into cell %05d (level %d)", t, level);
        }
    }
}

void draw_add(const float me[3], bool have_pos, bool have_heading, float heading) {
    if (!ImGui::TreeNodeEx("Add an object (experimental)", ImGuiTreeNodeFlags_DefaultOpen)) return;
    // copies of firs (mods/world_test (removed 2026-10-27, in git history)) render, but with a stray white bar across the scene (2026-10-27, unknown
    // cause); moves, turns, scales and removes are written independently of them
    ImGui::TextColored(ImVec4(1, 0.75f, 0.35f, 1), "Experimental: copies may show visual artifacts. Moves, turns, "
                       "scales and removes do not depend on them (WORLD_EDITOR_ADDS=0 leaves copies out).");
    ImGui::SetNextItemWidth(200);
    ImGui::InputTextWithHint("##filter", "filter, e.g. sapin, rock, kick", g_filter, sizeof g_filter);
    ImGui::SameLine();
    ImGui::Checkbox("favourites only", &g_favs_only);
    std::vector<int> shown;
    std::string f = lower(g_filter);
    for (int i = 0; i < (int)g_ix->props.size(); ++i) {
        const Prop &p = g_ix->props[i];
        if ((g_favs_only && !p.fav) || (!f.empty() && lower(p.name).find(f) == std::string::npos)) continue;
        shown.push_back(i);
    }
    if (ImGui::BeginListBox("##props", ImVec2(-FLT_MIN, 150))) {
        ImGuiListClipper clip;
        clip.Begin((int)shown.size());
        while (clip.Step())
            for (int r = clip.DisplayStart; r < clip.DisplayEnd; ++r) {
                const Prop &p = g_ix->props[shown[r]];
                char label[300];
                snprintf(label, sizeof label, "%s%s  (%.0fx%.0fx%.0f m, %d placed)##%d", p.fav ? "* " : "",
                         p.name.c_str(), p.size[0], p.size[1], p.size[2], p.count, shown[r]);
                if (ImGui::Selectable(label, g_prop == shown[r])) g_prop = shown[r];
            }
        ImGui::EndListBox();
    }
    ImGui::TextDisabled("%d of %d placeable props", (int)shown.size(), (int)g_ix->props.size());
    ImGui::SliderFloat("metres in front", &g_ahead, 0, 40, "%.1f");
    ImGui::SliderFloat("height offset", &g_dz, -10, 10, "%.1f m");
    ImGui::SliderFloat("turn", &g_add_yaw, -180, 180, "%.0f deg");
    ImGui::SliderFloat("scale", &g_add_scale, 0.25f, 4, "%.2f");
    float at[3];
    ahead_of(me, at, g_ahead, have_heading, heading);
    int level, cell = target_cell(*g_ix, at[0], at[1], level);
    bool ok = have_pos && g_prop >= 0 && g_prop < (int)g_ix->props.size() && cell >= 0;
    ImGui::BeginDisabled(!ok);
    if (ImGui::Button("Add here")) {
        const Prop &p = g_ix->props[g_prop];
        const Obj &ex = g_ix->objs[p.example];
        json j = {{"map", MAPS[g_map]}, {"op", "add"}, {"name", p.name},
                  {"src", {{"layer", ex.layer}, {"cell", ex.cell}, {"id", hex(ex.id)}}},
                  {"pos", jpos(at)}, {"yaw", r3(g_add_yaw)}, {"scale", r3(g_add_scale)}};
        request("POST", "edits", j, "add");
    }
    ImGui::EndDisabled();
    ImGui::SameLine();
    if (!have_pos) ImGui::TextDisabled("(no position)");
    else if (cell < 0) ImGui::TextDisabled("(no cell of %s there)", MAPS[g_map]);
    else
        ImGui::TextDisabled("at (%.1f, %.1f, %.1f)%s", at[0], at[1], at[2],
                            have_heading ? "" : ", north of you: ride a few metres to set the heading");
    ImGui::TreePop();
}

void draw_selected(const Near &s, const float me[3], bool have_pos, bool have_heading, float heading) {
    ImGui::PushID("sel");
    ImGui::Text("%s%s", s.name.c_str(), s.added ? "  (added)" : "");
    if (!s.added) {
        ImGui::SameLine();
        ImGui::TextDisabled("  %s, cell %05d%s", hex(s.obj->id).c_str(), s.obj->cell, s.obj->group ? ", group" : "");
    }
    ImGui::Text("at (%.2f, %.2f, %.2f), turned %.0f deg, scale %.2f", s.pos[0], s.pos[1], s.pos[2], s.yaw, s.scale);
    ImGui::SetNextItemWidth(260);
    ImGui::InputFloat3("##pos", g_sel_pos, "%.2f");
    ImGui::SameLine();
    auto edit = [&](const char *op, const char *key, json value) {
        json j = target_of(s);
        j["op"] = op;
        if (key) j[key] = std::move(value);
        request("POST", "edits", j, op);
    };
    if (ImGui::Button("Move")) edit("move", "pos", jpos(g_sel_pos));
    ImGui::SameLine();
    ImGui::BeginDisabled(!have_pos);
    if (ImGui::Button("to me")) memcpy(g_sel_pos, me, sizeof g_sel_pos);
    ImGui::SameLine();
    if (ImGui::Button("to in front")) ahead_of(me, g_sel_pos, g_ahead, have_heading, heading);
    ImGui::EndDisabled();
    if (!s.added && g_ix->has_grid) {                    // shipped objects stay in their cell (streaming, culling)
        int lvl = 0; double x0 = 0, y0 = 0, size = 0;
        if (g_ix->grid.rect(s.obj->cell, lvl, x0, y0, size) &&
            (g_sel_pos[0] < x0 - 8 || g_sel_pos[0] > x0 + size + 8 || g_sel_pos[1] < y0 - 8 ||
             g_sel_pos[1] > y0 + size + 8))
            ImGui::TextColored(ImVec4(1, 0.7f, 0.3f, 1), "outside its cell (%.0f..%.0f, %.0f..%.0f): it may not show "
                               "there; remove it and add a copy instead", x0, x0 + size, y0, y0 + size);
    }
    auto turn = [&](float deg) { edit("turn", "deg", r3(deg)); };
    auto scale = [&](float f) {
        float after = s.scale * f;
        if (after >= 0.05f && after <= 20) edit("scale", "factor", r3(f));    // world.py's sane range
    };
    if (ImGui::Button("-90")) turn(-90);
    ImGui::SameLine(); if (ImGui::Button("-15")) turn(-15);
    ImGui::SameLine(); if (ImGui::Button("+15")) turn(15);
    ImGui::SameLine(); if (ImGui::Button("+90")) turn(90);
    ImGui::SameLine(); ImGui::SetNextItemWidth(70); ImGui::InputFloat("##turn", &g_turn, 0, 0, "%.0f");
    ImGui::SameLine(); if (ImGui::Button("Turn")) turn(std::max(-360.0f, std::min(360.0f, g_turn)));
    if (ImGui::Button("x0.8")) scale(0.8f);
    ImGui::SameLine(); if (ImGui::Button("x1.25")) scale(1.25f);
    ImGui::SameLine(); if (ImGui::Button("x2")) scale(2);
    ImGui::SameLine(); ImGui::SetNextItemWidth(70); ImGui::InputFloat("##factor", &g_factor, 0, 0, "%.2f");
    ImGui::SameLine(); if (ImGui::Button("Scale")) scale(std::max(0.05f, std::min(20.0f, g_factor)));
    ImGui::PushStyleColor(ImGuiCol_Button, ImVec4(0.6f, 0.15f, 0.15f, 1));
    if (ImGui::Button("Remove")) {
        edit("remove", nullptr, json());
        g_sel.clear();
    }
    ImGui::PopStyleColor();
    ImGui::PopID();
}

void draw_nearby(const float me[3], bool have_pos, bool have_heading, float heading) {
    if (!ImGui::TreeNodeEx("Objects near you", ImGuiTreeNodeFlags_DefaultOpen)) return;
    if (!have_pos) { ImGui::TextDisabled("(no position)"); ImGui::TreePop(); return; }
    ImGui::SliderFloat("radius", &g_radius, 5, 300, "%.0f m");
    ImGui::SameLine();
    ImGui::Checkbox("also cameras, anchors, FX", &g_all_kinds);
    std::vector<Near> rows;
    float r2 = g_radius * g_radius;
    auto dist2 = [&](const float p[3]) { float dx = p[0] - me[0], dy = p[1] - me[1]; return dx * dx + dy * dy; };
    for (const Obj &o : g_ix->objs) {
        if (o.prop.empty() && !g_all_kinds) continue;    // menu anchors, spawn points: moving them breaks things
        Near n;
        n.obj = &o;
        memcpy(n.pos, o.pos, sizeof n.pos);
        n.yaw = o.yaw;
        n.scale = o.scale;
        auto &ch = g_world.changes;
        auto it = ch.empty() ? ch.end() : ch.find(obj_key(o.layer, o.cell, o.id));
        if (it != ch.end()) {
            const Change &c = it->second;
            if (c.removed) continue;
            if (c.moved) memcpy(n.pos, c.pos, sizeof n.pos);
            n.yaw += c.yaw;
            n.scale *= c.scale;
            n.changed = true;
        }
        float d = dist2(n.pos);
        if (d > r2) continue;
        n.dist = std::sqrt(d);
        n.name = o.name.empty() ? hex(o.id) : o.name;
        rows.push_back(std::move(n));
    }
    for (const auto &kv : g_world.added) {
        const Added &a = kv.second;
        Near n;
        n.added = true;
        n.n = a.n;
        memcpy(n.pos, a.pos, sizeof n.pos);
        n.yaw = a.yaw;
        n.scale = a.scale;
        float d = dist2(n.pos);
        if (d > r2) continue;
        n.dist = std::sqrt(d);
        n.name = a.name.empty() ? a.src_id : a.name;
        rows.push_back(std::move(n));
    }
    std::sort(rows.begin(), rows.end(), [](const Near &a, const Near &b) { return a.dist < b.dist; });
    if (rows.size() > 200) rows.resize(200);
    const Near *sel = nullptr;
    if (ImGui::BeginTable("near", 3, ImGuiTableFlags_Borders | ImGuiTableFlags_RowBg | ImGuiTableFlags_ScrollY,
                          ImVec2(0, 180))) {
        ImGui::TableSetupColumn("Object");
        ImGui::TableSetupColumn("m", ImGuiTableColumnFlags_WidthFixed, 50);
        ImGui::TableSetupColumn("", ImGuiTableColumnFlags_WidthFixed, 70);
        ImGui::TableHeadersRow();
        for (size_t i = 0; i < rows.size(); ++i) {
            const Near &n = rows[i];
            std::string key = n.added ? "a:" + std::to_string(n.n)
                                      : "o:" + std::to_string(obj_key(n.obj->layer, n.obj->cell, n.obj->id));
            ImGui::TableNextRow();
            ImGui::TableSetColumnIndex(0);
            bool is_sel = key == g_sel;
            if (is_sel) sel = &n;
            ImGui::PushID((int)i);
            if (ImGui::Selectable(n.name.c_str(), is_sel, ImGuiSelectableFlags_SpanAllColumns)) {
                g_sel = key;
                memcpy(g_sel_pos, n.pos, sizeof g_sel_pos);
            }
            ImGui::PopID();
            ImGui::TableSetColumnIndex(1);
            ImGui::Text("%.0f", n.dist);
            ImGui::TableSetColumnIndex(2);
            ImGui::TextDisabled("%s", n.added ? "added" : n.changed ? "changed" : n.obj->group ? "group" : "");
        }
        ImGui::EndTable();
    }
    if (sel) draw_selected(*sel, me, have_pos, have_heading, heading);
    else ImGui::TextDisabled("select an object to move, turn, scale or remove it");
    ImGui::TreePop();
}

const char *describe(const json &e, std::string &out) {
    std::string op = e.value("op", "?");
    std::string what;
    if (op == "add") what = e.value("name", "");
    else if (e.contains("of") && e["of"].is_number_integer()) what = "added #" + std::to_string(e["of"].get<int>());
    else if (e.contains("obj") && e["obj"].is_object())
        what = e["obj"].value("name", "").empty() ? e["obj"].value("id", "") : e["obj"].value("name", "");
    char extra[64] = "";
    if (op == "turn") snprintf(extra, sizeof extra, " %+.0f deg", num(e, "deg", 0));
    else if (op == "scale") snprintf(extra, sizeof extra, " x%.2f", num(e, "factor", 1));
    float p[3];
    if ((op == "move" || op == "add") && pos_of(e, p))
        snprintf(extra, sizeof extra, " (%.1f, %.1f, %.1f)", p[0], p[1], p[2]);
    out = "#" + std::to_string(e.value("n", 0)) + " " + op + " " + what + extra;
    return out.c_str();
}

// Live edits (live.cpp): what the session helper has turned into cells, and whether the game has read them yet.
void draw_live() {
    live::Status ls = live::status();
    int newest = 0;
    for (const json &e : g_edits) if (e.is_object()) newest = std::max(newest, e.value("n", 0));
    if (!ls.hooked || !ls.helper) {
        ImGui::TextDisabled("Edits show from the next game start.%s%s", ls.note.empty() ? "" : " ", ls.note.c_str());
        return;
    }
    if (ls.edits < newest)
        ImGui::TextColored(ImVec4(1, 1, 0.5f, 1), "live: writing the cells of edit #%d... (the helper is at #%d)",
                           newest, ls.edits);
    else
        ImGui::TextColored(ImVec4(0.7f, 0.9f, 0.7f, 1), "live: %d cell(s) replaced. Ride ~300 m away and back: an "
                           "edit shows when its cell streams in again.", (int)ls.cells.size());
    if (!ls.note.empty()) ImGui::TextDisabled("%s", ls.note.c_str());
    if (ls.cells.empty() || !ImGui::TreeNode("live cells")) return;
    for (const live::CellState &c : ls.cells)
        ImGui::BulletText("%s %s (%s): %s", c.map.c_str(), c.name.c_str(),
                          c.mode == "same" ? "same size" : "bigger: index redirected", c.state.c_str());
    if (!ls.io.empty()) ImGui::TextDisabled("%s", ls.io.c_str());
    ImGui::TreePop();
}

void draw_edits() {
    int count = 0;
    for (const json &e : g_edits) count += e.is_object() && e.value("map", "") == MAPS[g_map];
    char head[80];
    snprintf(head, sizeof head, "Edits of %s (%d)###edits", MAPS[g_map], count);
    if (!ImGui::TreeNodeEx(head, ImGuiTreeNodeFlags_DefaultOpen)) return;
    if (ImGui::Button("Refresh")) request("GET", "edits", json(), "refresh");
    ImGui::SameLine();
    int last = 0;
    for (const json &e : g_edits)
        if (e.is_object() && e.value("map", "") == MAPS[g_map]) last = std::max(last, e.value("n", 0));
    ImGui::BeginDisabled(last == 0);
    if (ImGui::Button("Undo last")) request("POST", "undo", json{{"n", last}}, "undo");
    ImGui::SameLine();
    if (ImGui::Button("Clear all...")) ImGui::OpenPopup("clear?");
    ImGui::EndDisabled();
    if (ImGui::BeginPopup("clear?")) {
        ImGui::Text("Remove all %d edits of %s?", count, MAPS[g_map]);
        if (ImGui::Button("Yes, clear them")) {
            request("POST", "clear", json{{"map", MAPS[g_map]}}, "clear");
            ImGui::CloseCurrentPopup();
        }
        ImGui::SameLine();
        if (ImGui::Button("No")) ImGui::CloseCurrentPopup();
        ImGui::EndPopup();
    }
    if (ImGui::BeginChild("edits", ImVec2(0, 140), ImGuiChildFlags_Borders)) {
        std::string text;
        for (auto it = g_edits.rbegin(); it != g_edits.rend(); ++it) {
            const json &e = *it;
            if (!e.is_object() || e.value("map", "") != MAPS[g_map]) continue;
            int n = e.value("n", 0);
            ImGui::PushID(n);
            if (ImGui::SmallButton("undo")) request("POST", "undo", json{{"n", n}}, "undo");
            ImGui::SameLine();
            ImGui::TextUnformatted(describe(e, text));
            ImGui::PopID();
        }
    }
    ImGui::EndChild();
    draw_live();
    ImGui::TreePop();
}

void panel(void *) {
    if (!g_asked) { g_asked = true; request("GET", "edits", json(), "load"); }
    float me[3];
    bool have_pos, have_heading, busy;
    float heading;
    std::string status;
    {
        std::lock_guard<std::mutex> lk(g_m);
        memcpy(me, g_pos, sizeof me);
        have_pos = g_have_pos;
        have_heading = g_have_heading;
        heading = g_heading;
        busy = g_busy;
        status = g_status;
        if (g_net_ver != g_edits_ver) {
            g_edits = g_net_edits;
            g_edits_ver = g_net_ver;
            g_world_dirty = true;
            if (!g_map_chosen && !g_edits.empty()) {           // start on the map of the newest edit
                std::string m = g_edits.back().value("map", "");
                for (int i = 0; i < NMAPS; ++i) if (m == MAPS[i]) g_map = i;
            }
        }
        if (g_loaded && g_loaded->map == MAPS[g_map] && g_ix != g_loaded) {
            g_ix = g_loaded;
            g_map_loaded = g_map;
            g_prop = -1;
            g_world_dirty = true;
        }
    }
    ImGui::SetNextItemWidth(140);
    if (ImGui::Combo("map", &g_map, MAPS, NMAPS)) { g_map_chosen = true; g_sel.clear(); g_world_dirty = true; }
    ImGui::SameLine();
    ImGui::TextDisabled("(pick the map you ride in)");
    bool want = g_map_loaded != g_map;
    if (want) {
        std::lock_guard<std::mutex> lk(g_m);
        if (!g_loading) {
            g_loading = true;
            std::string map = MAPS[g_map];
            std::thread([map] {
                auto ix = load_index(map);
                std::lock_guard<std::mutex> lk2(g_m);
                g_loaded = ix;
                g_loading = false;
            }).detach();
        }
    }
    if (g_world_dirty) { g_world = fold(g_edits, MAPS[g_map]); g_world_dirty = false; }
    ImGui::TextColored(busy ? ImVec4(1, 1, 0.5f, 1) : ImVec4(0.7f, 0.9f, 0.7f, 1), "%s", status.c_str());
    if (!g_ix || g_map_loaded != g_map) {
        draw_position(me, have_pos, have_heading, heading);
        ImGui::TextDisabled("loading the %s index...", MAPS[g_map]);
        return;
    }
    draw_position(me, have_pos, have_heading, heading);
    if (!g_ix->error.empty()) {
        ImGui::TextColored(ImVec4(1, 0.6f, 0.4f, 1), "%s", g_ix->error.c_str());
    } else {
        draw_add(me, have_pos, have_heading, heading);
        draw_nearby(me, have_pos, have_heading, heading);
    }
    draw_edits();
}

} // namespace

extern "C" POWDER_EXPORT int powder_mod_init(const PowderApi *api, PowderMod *mod) {
    if (api->version < 2 || api->size < sizeof(PowderApi)) return 1;    // on_frame + http
    g_api = api;
    g_mod = mod;
    if (!powder_imgui_attach(api)) { api->log(mod, "ImGui version mismatch: no panel"); return 1; }
    wchar_t self[MAX_PATH];
    HMODULE me;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                       (LPCWSTR)&powder_mod_init, &me);
    GetModuleFileNameW(me, self, MAX_PATH);
    g_dir = self;
    g_dir = g_dir.substr(0, g_dir.find_last_of(L'\\') + 1) + L"world_editor\\";   // powder_mods\world_editor\ .
    std::wstring game = g_dir.substr(0, g_dir.size() - 1);                         // ...\powder_mods\world_editor
    game = game.substr(0, game.find_last_of(L'\\'));                               // ...\powder_mods
    game = game.substr(0, game.find_last_of(L'\\') + 1);                           // Steep's folder
    live::init(api, mod, g_dir, game);
    api->add_panel(mod, "World editor", panel, nullptr);
    api->on_frame(mod, frame, nullptr);
    api->log(mod, "World editor panel ready (Mods tab)");
    return 0;
}
