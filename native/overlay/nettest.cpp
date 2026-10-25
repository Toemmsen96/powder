// Standalone check of the overlay's networking against a Powder server, without the game. Built from the same
// http.cpp the overlay uses. Run under Wine against a scratch Rust server to prove login -> players -> grant
// -> broadcast work (the same flow the Server tab drives):
//
//   wine native/bin/nettest.exe <base-url> <email> <password> [grant-profile-id]
//
// e.g. wine native/bin/nettest.exe https://127.0.0.1:18443 admin@powder.test admin1234 <rider-id>
#include "http.h"
#include "json.hpp"
#include <cstdio>
#include <string>

using json = nlohmann::json;

static std::string err_of(const pw::HttpResponse &r) {
    if (r.status == 0) return r.error;
    try { auto j = json::parse(r.body); if (j.contains("error")) return j["error"].get<std::string>(); } catch (...) {}
    return "HTTP " + std::to_string(r.status);
}

int main(int argc, char **argv) {
    if (argc < 4) { printf("usage: nettest <base-url> <email> <password> [grant-profile-id]\n"); return 2; }
    std::string base = argv[1], email = argv[2], pass = argv[3];
    std::string grant_id = argc > 4 ? argv[4] : "";

    pw::HttpClient c(base);
    if (!c.valid()) { printf("FAIL: bad url\n"); return 1; }
    printf("server: %s\n", c.host().c_str());

    json lb = {{"email", email}, {"password", pass}};
    auto r = c.post("/_powder/admin/api/login", lb.dump());
    if (!r.ok()) { printf("FAIL login: %s\n", err_of(r).c_str()); return 1; }
    printf("login ok: %s\n", r.body.c_str());

    r = c.get("/_powder/admin/api/players");
    if (!r.ok()) { printf("FAIL players: %s\n", err_of(r).c_str()); return 1; }
    try {
        auto j = json::parse(r.body);
        printf("players: %zu\n", j["players"].size());
        for (auto &p : j["players"])
            printf("  %-16s credits=%lld admin=%d\n", p.value("name", "").c_str(),
                   p.value("credits", (long long)0), (int)p.value("admin", false));
    } catch (const std::exception &e) { printf("FAIL parse players: %s\n", e.what()); return 1; }

    if (!grant_id.empty()) {
        json gb = {{"id", grant_id}, {"item", "credits"}, {"amount", 777}};
        r = c.post("/_powder/admin/api/grant", gb.dump());
        printf("grant: %s -> %s\n", r.ok() ? "ok" : "FAIL", r.body.c_str());
    }

    json bb = {{"text", "nettest hello"}, {"chat", false}};
    r = c.post("/_powder/admin/api/broadcast", bb.dump());
    printf("broadcast: %s -> %s\n", r.ok() ? "ok" : "FAIL", r.body.c_str());

    r = c.get("/_powder/admin/api/feed?since=0");
    printf("feed: %s -> %s\n", r.ok() ? "ok" : "(n/a)", r.status ? r.body.c_str() : err_of(r).c_str());

    printf("DONE\n");
    return 0;
}
