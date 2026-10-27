-- World editor, server half: keeps the list of world edits (objects added, moved, turned, scaled, removed) that
-- players make in the overlay's World editor panel (the client half, src/world_editor.cpp), and hands it to the
-- session step (apply.py) that turns it into edited world cells at the next game start. See README.md.
--
--   GET  /_powder/world_editor/edits   {"version": 1, "next": N, "edits": [edit, ...]}   (oldest first)
--   POST /_powder/world_editor/edits   body: one edit (below) -> the same, with the new edit numbered
--   POST /_powder/world_editor/undo    body: {"n": N} (default: the newest edit) -> the same; an added object's
--                                      own later edits go with it
--   POST /_powder/world_editor/clear   body: {"map": "Alpes"} -> the same, without that map's edits
--
-- An edit: {map, op, ...} with map one of MAPS and op
--   add     src = {layer, cell, id}  (a shipped entity that is copied), name, pos = {x, y, z}, yaw (deg), scale
--   move    pos = {x, y, z}          the object's new origin
--   turn    deg                      about Z, counter-clockwise seen from above
--   scale   factor                   about its origin
--   remove
-- For move/turn/scale/remove the object is either a shipped one, obj = {layer, cell, id, name}, or one an earlier
-- edit added, of = <that edit's n>. Positions are metres in the map's coordinates (Z up), ids "0x..." hex.
--
-- Stored with api.save (the server's database, scope mod:world_editor). The game's requests carry no session:
-- changes are taken only from this machine (loopback) unless ALLOW_REMOTE is set below, so on a server others can
-- reach, nobody else edits its world. Reading is open (the list is no secret: it is what the world looks like).

local ALLOW_REMOTE = false     -- true: anyone who can reach the server may change the world (no session exists)
local MAX_EDITS = 2000
local MAX_BODY = 4096
local MAPS = { Alpes = true, Japan = true, Alaska = true, WhiteRoom = true }
local OPS = { add = true, move = true, turn = true, scale = true, remove = true }

return function(api)
    local edits = api.load("edits", {})
    local next_n = api.load("next", 1)
    for _, e in ipairs(edits) do                  -- a list saved by an older copy without "next"
        if e.n >= next_n then next_n = e.n + 1 end
    end

    local function listing()
        local parts = {}
        for i, e in ipairs(edits) do parts[i] = api.json.encode(e) end
        return '{"version": 1, "next": ' .. next_n .. ', "edits": [' .. table.concat(parts, ", ") .. ']}'
    end

    local function reply(status, body)
        return { status = status, body = body, content_type = "application/json",
                 headers = { ["Cache-Control"] = "no-store" } }
    end

    local function fail(status, message)
        return reply(status, api.json.encode({ ok = false, error = message }))
    end

    local function store()
        api.save("edits", edits)
        api.save("next", next_n)
    end

    -- checks; each returns the clean value or nil, why
    local function number(v, lo, hi)
        if type(v) ~= "number" or v ~= v or v < lo or v > hi then return nil end
        return v + 0.0
    end

    local function integer(v, lo, hi)
        if math.type(v) == "float" and v == math.floor(v) then v = math.tointeger(v) end
        if math.type(v) ~= "integer" or v < lo or v > hi then return nil end
        return v
    end

    local function position(v)
        if type(v) ~= "table" then return nil end
        local out = {}
        for i = 1, 3 do
            out[i] = number(v[i], -20000, 20000)
            if not out[i] then return nil end
        end
        return out
    end

    local function text(v)
        if type(v) ~= "string" then return "" end
        return (string.gsub(string.sub(v, 1, 120), "[^%w_%-%.%s#]", "?"))
    end

    local function object_ref(v)
        if type(v) ~= "table" or type(v.id) ~= "string" or #v.id > 18 or not string.match(v.id, "^0x%x+$") then
            return nil
        end
        local layer, cell = integer(v.layer or 0, 0, 3), integer(v.cell, 0, 100000)
        if not layer or not cell then return nil end
        return { layer = layer, cell = cell, id = string.lower(v.id), name = text(v.name) }
    end

    local function find(n)
        for i, e in ipairs(edits) do
            if e.n == n then return i, e end
        end
    end

    local function check(body)
        if type(body) ~= "table" then return nil, "an edit is a JSON object" end
        local map, op = body.map, body.op
        if not MAPS[map] then return nil, "unknown map" end
        if not OPS[op] then return nil, "unknown op" end
        local e = { map = map, op = op, t = os.time() }
        if op == "add" then
            e.src = object_ref(body.src)
            e.pos = position(body.pos)
            e.yaw = number(body.yaw or 0, -360, 360)
            e.scale = number(body.scale or 1, 0.05, 20)
            e.name = text(body.name)
            if not e.src or not e.pos or not e.yaw or not e.scale then return nil, "add needs src, pos, yaw, scale" end
            return e
        end
        if body.of ~= nil then
            local of = integer(body.of, 1, math.maxinteger)
            local _, target = find(of or -1)
            if not target or target.op ~= "add" or target.map ~= map then return nil, "of: no such added object" end
            e.of = of
        else
            e.obj = object_ref(body.obj)
            if not e.obj then return nil, "obj needs layer, cell, id" end
        end
        if op == "move" then
            e.pos = position(body.pos)
            if not e.pos then return nil, "move needs pos" end
        elseif op == "turn" then
            e.deg = number(body.deg, -360, 360)
            if not e.deg then return nil, "turn needs deg" end
        elseif op == "scale" then
            e.factor = number(body.factor, 0.05, 20)
            if not e.factor then return nil, "scale needs factor" end
        end
        return e
    end

    local function body_of(req)
        if #req.body > MAX_BODY then return nil end
        local v = api.json.decode(req.body ~= "" and req.body or "{}")
        return v
    end

    local function local_peer(peer)
        return peer == "127.0.0.1" or peer == "::1" or string.match(peer or "", "^::ffff:127%.") ~= nil
    end

    api.before_http("*", "/_powder/world_editor/*", function(req)
        local path, method = req.path, req.method
        if path == "/_powder/world_editor/edits" and method == "GET" then
            return reply(200, listing())
        end
        local action = string.match(path, "^/_powder/world_editor/(%a+)$")
        if method ~= "POST" or not (action == "edits" or action == "undo" or action == "clear") then
            return fail(404, "not found")
        end
        if not ALLOW_REMOTE and not local_peer(req.peer) then
            return fail(403, "the world is edited from the server's own machine only")
        end
        local body = body_of(req)
        if type(body) ~= "table" then return fail(400, "the body is not a JSON object") end

        if action == "edits" then
            if #edits >= MAX_EDITS then return fail(409, "too many edits (" .. MAX_EDITS .. "): undo or clear some") end
            local e, why = check(body)
            if not e then return fail(400, why) end
            e.n = next_n
            next_n = next_n + 1
            edits[#edits + 1] = e
            store()
            api.log.info("edit", e.n, e.map, e.op, e.name or (e.obj and (e.obj.name ~= "" and e.obj.name or e.obj.id))
                         or ("of " .. tostring(e.of)))
        elseif action == "undo" then
            local n = body.n ~= nil and integer(body.n, 1, math.maxinteger) or (edits[#edits] or {}).n
            local i, e = find(n or -1)
            if not i then return fail(404, "no such edit") end
            local kept, gone = {}, 0
            for _, x in ipairs(edits) do
                if x.n == e.n or (e.op == "add" and x.of == e.n) then gone = gone + 1 else kept[#kept + 1] = x end
            end
            edits = kept
            store()
            api.log.info("undid edit", e.n, e.map, e.op, "(" .. gone .. " edit(s) gone)")
        else
            if not MAPS[body.map] then return fail(400, "unknown map") end
            local kept = {}
            for _, x in ipairs(edits) do
                if x.map ~= body.map then kept[#kept + 1] = x end
            end
            api.log.info("cleared", #edits - #kept, "edit(s) of", body.map)
            edits = kept
            store()
        end
        return reply(200, listing())
    end)
    api.log.info("world editor:", #edits, "edit(s) stored; next game start applies them (mods/world_editor)")
end
