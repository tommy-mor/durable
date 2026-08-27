-- app.lua — the HAND-COMPILED form of the .hop examples.
--
-- This file is what the hopc compiler will emit, written by hand to prove
-- the runtime before the compiler exists. Every rt.register block below
-- corresponds to a placement mark in durable/examples/netlua/*.hop; the
-- comments show the source it was "compiled" from. Loaded identically into
-- every VM — segments simply never run on the side they don't belong to.

-- ===========================================================================
-- handle_check.hop
-- ===========================================================================

-- server let taken = {...}
if SIDE == "server" then
  taken = { alice = true, bob = true, root = true }
end

-- fn check_handle(handle) {
--   dom.spinner(true);
--   server!();                      <- hop "check_handle:1", crossed: {handle}
--   let ok = !taken[handle];        (tail position → reply routes straight home)
--   browser!();
--   dom.spinner(false);
--   return ok;
-- }
rt.register("check_handle:1", function(vars)
  if vars.handle == "root" then
    error("reserved handle")            -- exceptions unwind through hops
  end
  return not taken[vars.handle]
end)

function check_handle(handle)           -- origin segments 0 and 2
  print("check '" .. handle .. "' ...")
  local ok = rt.at("server", "check_handle:1", { handle = handle })
  print("  -> '" .. handle .. "' is " .. (ok and "available" or "taken"))
  return ok
end

-- fn check_handle_safely(handle) { try { ... } catch { ... } }
function check_handle_safely(handle)
  local ok, res = pcall(check_handle, handle)
  if ok then
    return res
  end
  print("  caught server-side error: " .. tostring(res))
  return false
end

-- ===========================================================================
-- delete_account: nested hops, browser → server → browser → server.
-- Exercises the LIFO stack: while the origin's flow coroutine is suspended
-- waiting on the server, the *same browser* runs a sub-segment for the
-- same flow (the confirm dialog), and replies route to the right one.
-- ===========================================================================

-- fn confirm(n) { browser!(); return dom.confirm("really delete " .. n); }
rt.register("confirm:1", function(vars)
  print("  [dialog] really delete " .. vars.n .. " items? (user clicks yes)")
  return true
end)

-- fn delete_account() {
--   server!();                      <- hop "delete_account:1"
--   let n = count_user_data(session());
--   if confirm(n) { purge(); return "deleted"; } else { return "kept"; }
-- }
rt.register("delete_account:1", function(vars)
  local n = 3                            -- pretend: count_user_data(session())
  local yes = rt.at(rt.session(), "confirm:1", { n = n })
  if yes then
    print("purged data for session " .. rt.session())
    return "deleted " .. n .. " items"
  end
  return "kept"
end)

function delete_account()
  print("account: " .. rt.at("server", "delete_account:1", {}))
end

-- ===========================================================================
-- whiteboard.hop — casts and fan-out.
-- ===========================================================================

-- server let members = {}  (session id -> color, assigned on connect)
if SIDE == "server" then
  members = {}
  palette = { "tomato", "steelblue", "seagreen", "orchid" }
  n_members = 0
end

rt.register("wb_connect:1", function(vars)
  n_members = n_members + 1
  members[rt.session()] = palette[(n_members - 1) % #palette + 1]
  print("session " .. rt.session() .. " joins as " .. members[rt.session()])
end)

-- fn stroke(from, to) {
--   canvas.draw(from, to, "silver");
--   server!();                      <- cast (one-way): "stroke:1"
--   let color = members[session()];
--   cast browsers { canvas.draw(from, to, color); }   <- "stroke:2"
-- }
rt.register("stroke:1", function(vars)
  local color = members[rt.session()]
  rt.cast("browsers", "stroke:2",
          { from = vars.from, to = vars.to, color = color, who = rt.session() })
end)

rt.register("stroke:2", function(vars)
  local note = rt.session() == vars.who and "  (my echo, repainted)" or ""
  print("draw " .. vars.from .. " -> " .. vars.to .. " in " .. vars.color .. note)
end)

function wb_join()
  rt.cast("server", "wb_connect:1", {})
end

function stroke(from, to)
  print("draw " .. from .. " -> " .. to .. " in silver (optimistic echo)")
  rt.cast("server", "stroke:1", { from = from, to = to })
end

-- ===========================================================================
-- Demo entry points, invoked by the host on specific browser VMs.
-- Each rt.start_flow is "a DOM event fired".
-- ===========================================================================

function demo_flows()
  rt.start_flow(function() check_handle_safely("carol") end)  -- available
  rt.start_flow(function() check_handle_safely("alice") end)  -- taken
  rt.start_flow(function() check_handle_safely("root") end)   -- server error
  rt.start_flow(function() delete_account() end)              -- nested hops
end

function demo_join()
  rt.start_flow(function() wb_join() end)
end

function demo_stroke()
  rt.start_flow(function() stroke("(1,1)", "(2,3)") end)
end
