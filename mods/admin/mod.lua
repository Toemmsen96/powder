-- Admin page: a web page for the server's admins, at https://<server>/_powder/admin
--   players and who is online (game / Ubisoft Connect), level and credits
--   a player's inventory, grant or remove credits and items
--   a player's stats (level, XP, ...): show and edit, reset a player
--   a message to everyone online (websocket notification, optionally a Ubisoft Connect chat message)
--
-- Only server admins can use it: list their profile ids in config.toml (`powder-server admin accounts` shows
-- them) and sign in on the page with that account's email and password:
--   [server]
--   admins = ["<profile id>"]
-- Without admins configured the mod does nothing. Needs the Rust server (api.admin); the Python server does
-- not run Lua mods.
--
-- Reaching the page: from the server's PC, https://127.0.0.1:<https_port>/_powder/admin (the browser does not
-- know the server's local CA: accept the warning, or import data/tls/ca.pem). Under linux/powder-wrap the
-- server lives in the session's own network: run it outside the wrapper to use the page.
--
-- Security: the session is a random token in a cookie (HttpOnly, Secure, SameSite=Strict, 8 h, kept in
-- memory: a restart signs everyone out); every change also needs the X-Powder-Admin header and a same-origin
-- Origin, so another web page cannot use an admin's session. Sign-ins count against the server's rate limit.

local SESSION_S = 8 * 3600
local MAX_SESSIONS = 16
local COOKIE = "powder_admin"
local MAX_MESSAGE = 500

-- ---- the page --------------------------------------------------------------------------------------------
local PAGE = [==[<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Powder admin</title>
<link rel="stylesheet" href="/_powder/admin/app.css">
<script src="/_powder/admin/app.js" defer></script>
</head>
<body>
<header><h1>Powder admin</h1><span id="who"></span><button id="logout" hidden>Sign out</button></header>
<main>
  <form id="login" hidden>
    <h2>Sign in</h2>
    <p>With the email and password of an account listed in <code>[server] admins</code>.</p>
    <label>Email <input name="email" type="email" autocomplete="username" required></label>
    <label>Password <input name="password" type="password" autocomplete="current-password" required></label>
    <button>Sign in</button>
  </form>
  <section id="panel" hidden>
    <h2>Message to everyone online</h2>
    <form id="broadcast" class="row">
      <input name="text" maxlength="500" placeholder="Server restarts in 5 minutes" required>
      <label class="inline"><input name="chat" type="checkbox"> also as a Ubisoft Connect chat message</label>
      <button>Send</button>
    </form>
    <h2>Players <button id="refresh" class="small">Refresh</button></h2>
    <table id="players"><thead><tr><th>Name</th><th>Online</th><th>Level</th><th>Credits</th><th>Email</th>
      <th>Since</th><th></th></tr></thead><tbody></tbody></table>
    <section id="player" hidden>
      <h2 id="player-title"></h2>
      <form id="grant" class="row">
        <input name="item" placeholder="credits or an item id" required>
        <input name="amount" type="number" step="1" placeholder="amount (negative removes)" required>
        <button>Grant</button>
      </form>
      <details><summary>Inventory</summary><table id="inventory"><thead><tr><th>Item</th><th>Quantity</th>
        <th>Changed</th></tr></thead><tbody></tbody></table></details>
      <details open><summary>Stats</summary>
        <input id="stat-filter" placeholder="filter, e.g. level">
        <table id="stats"><thead><tr><th>Key</th><th>Value</th><th></th></tr></thead><tbody></tbody></table>
      </details>
      <p><button id="reset" class="danger">Reset this player</button> (they start again as a new player;
        their account stays)</p>
    </section>
  </section>
  <p id="status" role="status"></p>
</main>
</body>
</html>
]==]

local STYLE = [==[
:root { --bg: #f6f7f9; --fg: #1d2330; --muted: #5d6676; --line: #d9dde4; --accent: #2563eb; --danger: #b42318;
        --card: #fff; color-scheme: light dark; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #14171c; --fg: #e6e9ee; --muted: #9aa3b2; --line: #2c323c; --accent: #6ea0ff; --danger: #ff7b6e;
          --card: #1b1f26; }
}
body { margin: 0; font: 15px/1.45 system-ui, sans-serif; background: var(--bg); color: var(--fg); }
header { display: flex; gap: 1rem; align-items: center; padding: .8rem 1.2rem; border-bottom: 1px solid var(--line); }
header h1 { font-size: 1.15rem; margin: 0; flex: 1; }
main { max-width: 1100px; margin: 0 auto; padding: 1rem 1.2rem 3rem; }
h2 { font-size: 1rem; margin: 1.6rem 0 .6rem; display: flex; gap: .6rem; align-items: center; }
form#login { max-width: 360px; display: grid; gap: .6rem; }
label { display: grid; gap: .2rem; color: var(--muted); }
label.inline { display: flex; align-items: center; gap: .3rem; }
input { font: inherit; padding: .4rem .5rem; border: 1px solid var(--line); border-radius: 6px; background: var(--card);
        color: var(--fg); min-width: 0; }
button { font: inherit; padding: .4rem .8rem; border: 1px solid var(--accent); background: var(--accent); color: #fff;
         border-radius: 6px; cursor: pointer; }
button.small, td button { padding: .15rem .5rem; font-size: .85rem; }
button.danger { background: none; color: var(--danger); border-color: var(--danger); }
.row { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; }
.row input:first-child { flex: 1 1 240px; }
table { width: 100%; border-collapse: collapse; background: var(--card); border: 1px solid var(--line); }
th, td { text-align: left; padding: .35rem .55rem; border-bottom: 1px solid var(--line); vertical-align: middle; }
th { color: var(--muted); font-weight: 600; font-size: .85rem; }
td.num { font-variant-numeric: tabular-nums; }
tr.selected td { background: color-mix(in srgb, var(--accent) 12%, transparent); }
.badge { font-size: .78rem; padding: .05rem .4rem; border-radius: 99px; border: 1px solid var(--line); color: var(--muted);
         margin-right: .2rem; }
.badge.on { border-color: var(--accent); color: var(--accent); }
#stats input { width: 9rem; }
#stat-filter { margin: .4rem 0; }
#status { color: var(--muted); min-height: 1.4em; }
#status.error { color: var(--danger); }
code { font-size: .9em; }
details { margin: .6rem 0; }
@media (max-width: 700px) { #players th:nth-child(5), #players td:nth-child(5), #players th:nth-child(6),
  #players td:nth-child(6) { display: none; } }
]==]

local SCRIPT = [==[
"use strict";
const $ = (sel) => document.querySelector(sel);
let selected = null;

function status(text, error) {
  const el = $("#status");
  el.textContent = text || "";
  el.className = error ? "error" : "";
}

async function call(name, body) {
  const opts = body === undefined ? {} : {
    method: "POST", body: JSON.stringify(body),
    headers: { "Content-Type": "application/json", "X-Powder-Admin": "1" },
  };
  const res = await fetch("/_powder/admin/api/" + name, { credentials: "same-origin", ...opts });
  let data = {};
  try { data = await res.json(); } catch (e) { /* not JSON */ }
  if (res.status === 401 && name !== "login") { showLogin(); }
  if (!res.ok) { throw new Error(data.error || ("HTTP " + res.status)); }
  return data;
}

function cell(row, text, cls) {
  const td = document.createElement("td");
  td.textContent = text === null || text === undefined ? "" : String(text);
  if (cls) td.className = cls;
  row.appendChild(td);
  return td;
}

function badge(td, text, on) {
  const b = document.createElement("span");
  b.className = "badge" + (on ? " on" : "");
  b.textContent = text;
  td.appendChild(b);
}

function list(x) { return Array.isArray(x) ? x : []; }

function showLogin() {
  $("#login").hidden = false; $("#panel").hidden = true; $("#logout").hidden = true; $("#who").textContent = "";
}

async function loadPlayers() {
  const data = await call("players");
  $("#login").hidden = true; $("#panel").hidden = false; $("#logout").hidden = false;
  $("#who").textContent = "signed in as " + data.me.name;
  const body = $("#players tbody");
  body.replaceChildren();
  for (const p of list(data.players)) {
    const tr = document.createElement("tr");
    if (selected && selected.profile_id === p.profile_id) tr.className = "selected";
    const name = cell(tr, p.name);
    if (p.admin) badge(name, "admin", true);
    const online = cell(tr, "");
    badge(online, "game", p.game); badge(online, "Connect", p.connect);
    cell(tr, p.level, "num"); cell(tr, p.credits, "num"); cell(tr, p.email || "(no account)");
    cell(tr, (p.created_at || "").slice(0, 10));
    const btn = document.createElement("button");
    btn.textContent = "Open";
    btn.addEventListener("click", () => openPlayer(p));
    cell(tr, "").appendChild(btn);
    body.appendChild(tr);
  }
}

async function openPlayer(p) {
  selected = p;
  const data = await call("player?id=" + encodeURIComponent(p.profile_id));
  $("#player").hidden = false;
  $("#player-title").textContent = p.name + " (" + p.profile_id + ")";
  const inv = $("#inventory tbody");
  inv.replaceChildren();
  for (const i of list(data.inventory.items)) {
    const tr = document.createElement("tr");
    cell(tr, i.item_id); cell(tr, i.quantity, "num"); cell(tr, i.last_modified);
    inv.appendChild(tr);
  }
  renderStats(list(data.stats));
  await loadPlayers();
}

function renderStats(stats) {
  const filter = $("#stat-filter").value.toLowerCase();
  const body = $("#stats tbody");
  body.replaceChildren();
  for (const s of stats) {
    if (filter && !s.key.toLowerCase().includes(filter)) continue;
    const tr = document.createElement("tr");
    cell(tr, s.key);
    const input = document.createElement("input");
    input.value = String(s.value);
    cell(tr, "").appendChild(input);
    const btn = document.createElement("button");
    btn.textContent = "Set";
    btn.addEventListener("click", () => run(async () => {
      await call("stat", { id: selected.profile_id, key: s.key, value: input.value, double: s.double });
      status(s.key + " set to " + input.value);
    }));
    cell(tr, "").appendChild(btn);
    body.appendChild(tr);
  }
  $("#stat-filter").oninput = () => renderStats(stats);
}

async function run(fn) {
  try { await fn(); } catch (e) { status(e.message, true); }
}

$("#login").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const f = new FormData(ev.target);
  run(async () => {
    await call("login", { email: f.get("email"), password: f.get("password") });
    ev.target.reset(); status(""); await loadPlayers();
  });
});

$("#logout").addEventListener("click", () => run(async () => { await call("logout", {}); showLogin(); }));
$("#refresh").addEventListener("click", () => run(loadPlayers));

$("#grant").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const f = new FormData(ev.target);
  run(async () => {
    const r = await call("grant", { id: selected.profile_id, item: f.get("item"), amount: Number(f.get("amount")) });
    status(r.result.item_id + ": " + r.result.before + " -> " + r.result.after);
    await openPlayer(selected);
  });
});

$("#reset").addEventListener("click", () => {
  if (!selected || !confirm("Reset " + selected.name + "? Their progress is deleted.")) return;
  run(async () => {
    await call("reset", { id: selected.profile_id });
    status(selected.name + " was reset");
    $("#player").hidden = true; selected = null; await loadPlayers();
  });
});

$("#broadcast").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const f = new FormData(ev.target);
  run(async () => {
    const r = await call("broadcast", { text: f.get("text"), chat: f.get("chat") === "on" });
    status("sent to " + r.result.websockets + " websocket(s), " + r.result.chats + " Connect chat(s)");
    ev.target.reset();
  });
});

run(loadPlayers);
]==]

return function(api)
    if not api.admin.configured then
        api.log.info("no [server] admins configured: the admin page is off")
        return
    end

    local sessions = {}   -- token -> { pid, name, expires }

    local SECURITY_HEADERS = {
        ["Cache-Control"] = "no-store",
        ["X-Frame-Options"] = "DENY",
        ["X-Content-Type-Options"] = "nosniff",
        ["Referrer-Policy"] = "no-referrer",
        ["Content-Security-Policy"] = "default-src 'none'; script-src 'self'; style-src 'self'; "
            .. "connect-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    }

    local function reply(status, body, content_type, extra)
        local headers = {}
        for k, v in pairs(SECURITY_HEADERS) do headers[k] = v end
        for k, v in pairs(extra or {}) do headers[k] = v end
        return { status = status, body = body, content_type = content_type, headers = headers }
    end

    local function json(status, data, extra)
        return reply(status, api.json.encode(data), "application/json; charset=utf-8", extra)
    end

    local function fail(status, message)
        return json(status, { error = message })
    end

    local function cookie_of(req)
        local header = req.headers["cookie"] or ""
        for name, value in string.gmatch(header, "([^=;%s]+)=([^;]*)") do
            if name == COOKIE then return value end
        end
        return nil
    end

    local function current_admin(req)
        local token = cookie_of(req)
        local s = token and sessions[token]
        if not s then return nil end
        if s.expires < os.time() or not api.admin.is_admin(s.pid) then
            sessions[token] = nil
            return nil
        end
        return s, token
    end

    local function new_session(pid, name)
        local count, oldest, oldest_token = 0, math.huge, nil
        for t, s in pairs(sessions) do
            count = count + 1
            if s.expires < oldest then oldest, oldest_token = s.expires, t end
        end
        if count >= MAX_SESSIONS and oldest_token then sessions[oldest_token] = nil end
        local token = api.admin.token()
        sessions[token] = { pid = pid, name = name, expires = os.time() + SESSION_S }
        return token
    end

    local function set_cookie(token, max_age)
        return { ["Set-Cookie"] = string.format("%s=%s; Path=/_powder/admin; Max-Age=%d; Secure; HttpOnly; "
            .. "SameSite=Strict", COOKIE, token, max_age) }
    end

    -- a change: our own page's fetch (custom header + same origin), never a form or another site
    local function same_origin(req)
        if req.headers["x-powder-admin"] ~= "1" then return false end
        local origin = req.headers["origin"]
        if origin == nil then return true end
        local host = req.headers["host"] or ""
        return origin == "https://" .. host
    end

    local function body_of(req)
        local data = api.json.decode(req.body ~= "" and req.body or "{}")
        return type(data) == "table" and data or {}
    end

    local function query_param(req, name)
        for k, v in string.gmatch(req.query or "", "([^&=]+)=([^&]*)") do
            if k == name then return v end
        end
        return nil
    end

    local function result(value, err)
        if value == nil or value == false then return fail(400, err or "refused") end
        return json(200, { ok = true, result = value })
    end

    local actions = {}

    function actions.players(req, admin)
        return json(200, { me = { profile_id = admin.pid, name = admin.name }, players = api.admin.players() })
    end

    function actions.player(req)
        local id = query_param(req, "id") or ""
        local inventory, err = api.admin.inventory(id)
        if not inventory then return fail(404, err) end
        return json(200, { inventory = inventory, stats = api.admin.stats(id) })
    end

    function actions.grant(req, admin, body)
        local amount = math.tointeger(tonumber(body.amount))
        if not amount or amount == 0 then return fail(400, "the amount must be a whole number, not 0") end
        api.log.info(admin.name, "grants", tostring(body.item), amount, "to", tostring(body.id))
        return result(api.admin.grant(tostring(body.id), tostring(body.item or ""), amount))
    end

    function actions.stat(req, admin, body)
        local value = tonumber(body.value)
        if value == nil then return fail(400, "the value must be a number") end
        if body.double then value = value + 0.0 else value = math.tointeger(value) end
        if value == nil then return fail(400, "an integer stat needs a whole number") end
        api.log.info(admin.name, "sets", tostring(body.key), "=", value, "for", tostring(body.id))
        return result(api.admin.set_stat(tostring(body.id), tostring(body.key or ""), value))
    end

    function actions.reset(req, admin, body)
        api.log.warn(admin.name, "resets player", tostring(body.id))
        return result(api.admin.reset(tostring(body.id)))
    end

    function actions.broadcast(req, admin, body)
        local text = tostring(body.text or "")
        if text == "" or #text > MAX_MESSAGE then return fail(400, "a message of 1-" .. MAX_MESSAGE .. " characters") end
        local sockets, chats = 0, 0
        for _, p in ipairs(api.admin.players()) do
            if p.game or p.connect then
                sockets = sockets + api.admin.notify(p.profile_id, "POWDER_ADMIN_MESSAGE",
                                                     { message = text, from = admin.name })
                if body.chat and p.connect and p.profile_id ~= admin.pid then
                    if api.admin.chat(admin.pid, p.profile_id, text) then chats = chats + 1 end
                end
            end
        end
        api.log.info(admin.name, "broadcast to", sockets, "websocket(s),", chats, "chat(s):", text)
        return json(200, { ok = true, result = { websockets = sockets, chats = chats } })
    end

    local READS = { players = true, player = true }

    api.before_http("*", "/_powder/admin*", function(req)
        local path, method = req.path, req.method
        if path == "/_powder/admin" or path == "/_powder/admin/" then
            return reply(200, PAGE, "text/html; charset=utf-8")
        elseif path == "/_powder/admin/app.js" then
            return reply(200, SCRIPT, "text/javascript; charset=utf-8")
        elseif path == "/_powder/admin/app.css" then
            return reply(200, STYLE, "text/css; charset=utf-8")
        end

        local name = string.match(path, "^/_powder/admin/api/([%w_]+)$")
        if not name then return fail(404, "not found") end
        if method ~= "GET" and not same_origin(req) then return fail(403, "refused: not from the admin page") end

        if name == "login" and method == "POST" then
            local body = body_of(req)
            local who, err = api.admin.sign_in(tostring(body.email or ""), tostring(body.password or ""), req.peer)
            if not who then return fail(401, err) end
            return json(200, { ok = true, name = who.name }, set_cookie(new_session(who.profile_id, who.name), SESSION_S))
        end

        local admin, token = current_admin(req)
        if not admin then return fail(401, "sign in first") end
        if name == "logout" and method == "POST" then
            sessions[token] = nil
            return json(200, { ok = true }, set_cookie("", 0))
        end
        local action = actions[name]
        if not action or (READS[name] and method ~= "GET") or (not READS[name] and method ~= "POST") then
            return fail(404, "not found")
        end
        return action(req, admin, body_of(req))
    end)
    api.log.info("admin page at /_powder/admin for", "the configured admin(s)")
end
