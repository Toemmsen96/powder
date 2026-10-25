-- Example mod (its folder starts with "_", so the server skips it; copy it to e.g. mods/my_mod/mod.lua,
-- or drop a single mods/my_mod.lua with the same contents).
--
-- A mod hands the server a setup function. The recommended form returns it:

return function(api)
    api.log.info("example mod loaded (API v" .. api.version .. ") on", api.config.server.name)

    -- Watch an event. "purchase" carries { profile_id, offer_id, changes = { {item_id, delta, after}, ... } }.
    api.on("purchase", function(ev)
        for _, c in ipairs(ev.changes or {}) do
            api.log.info(string.sub(ev.profile_id, 1, 8), "item", c.item_id, "now", c.after)
        end
    end)

    -- Per-mod storage (SQLite, survives restarts). Count how many times this mod has loaded.
    local loads = (api.load("loads", 0)) + 1
    api.save("loads", loads)
    api.log.info("loaded", loads, "time(s) so far")

    -- Answer a NEX call yourself: return response bytes, or nil to let the server decide.
    -- api.before_rmc("GameStorageProtocol::GetGameStorageTime_V1", function(ctx, params)
    --     return nil
    -- end)

    -- Answer an HTTP request yourself: return a response table, or nil to continue.
    -- api.before_http("GET", "/v1/something/*", function(req)
    --     return { status = 200, body = "handled by a mod" }
    -- end)
end
