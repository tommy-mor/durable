-- hoprt.lua — the hop runtime. Loaded identically into every VM.
--
-- This is the layer the compiler will target. It knows nothing about
-- syntax: it sees segment functions registered under stable hop ids, and
-- moves flows between sides as data packets.
--
-- Globals provided by the host before this file loads:
--   SIDE     "server" | "browser"
--   SESSION  session id for browser VMs ("A", "B", ...); nil on the server
--   __send(packet)  hand a packet table to the transport
--
-- Packet shapes (all plain data; the wire never carries code):
--   { kind="call",  flow, to, hop, vars, origin, reply_to }
--   { kind="cast",  flow, to, hop, vars, origin }
--   { kind="reply", flow, to, value }
--   { kind="error", flow, to, err }

local hops = {}      -- hop id -> segment function(vars) -> value
local stacks = {}    -- flow id -> LIFO of suspended entries (nested hops)
local contexts = {}  -- coroutine -> { flow, origin }
local next_flow = 0

local function my_addr()
  if SIDE == "server" then return "server" else return SESSION end
end

-- Tag this VM's prints and route them through the host (__print) so all
-- VMs and the wire log share one ordered stdout — the merged transcript
-- reads like a distributed trace.
local label = SIDE == "server" and "[server   ]" or ("[browser " .. SESSION .. "]")
print = function(...)
  local parts = {}
  for i, v in ipairs({ ... }) do
    parts[i] = tostring(v)
  end
  __print(label .. " " .. table.concat(parts, " "))
end

rt = {}

function rt.register(id, fn)
  hops[id] = fn
end

local function ctx()
  local c = contexts[coroutine.running()]
  assert(c, "hop primitives may only be called inside a flow")
  return c
end

-- Session identity. On a browser: its own session. In a server segment:
-- the origin session of the flow. In the spike the origin rides the packet;
-- a real transport derives it from the connection so clients can't forge it.
function rt.session()
  if SIDE == "server" then return ctx().origin else return SESSION end
end

-- at: move execution to `target`, run segment `hop_id` with `vars`, suspend
-- this flow until the value (or error) comes back. This is what a placement
-- mark compiles into.
function rt.at(target, hop_id, vars)
  local c = ctx()
  __send({ kind = "call", flow = c.flow, to = target, hop = hop_id,
           vars = vars, origin = c.origin, reply_to = my_addr() })
  local ok, v = coroutine.yield()
  if not ok then
    -- re-raise on this side: exceptions unwind through hops
    error(v, 0)
  end
  return v
end

-- cast: move and don't wait. No reply routing, no error channel,
-- at-most-once. `target` may be "server", a session id, or "browsers"
-- (the transport fans that out).
function rt.cast(target, hop_id, vars)
  local c = ctx()
  __send({ kind = "cast", flow = c.flow, to = target, hop = hop_id,
           vars = vars, origin = c.origin })
end

-- Drive a coroutine one step and dispose of the outcome.
--   finished ok    → reply upstream (if a remote caller waits) or done
--   finished error → error upstream, or report at the flow's origin
--   suspended      → it called at(); park it until the reply resumes it
local function step(entry, ...)
  local co = entry.co
  local ok, res = coroutine.resume(co, ...)
  if coroutine.status(co) == "dead" then
    contexts[co] = nil
    if entry.on_complete == "reply" then
      if ok then
        __send({ kind = "reply", flow = entry.flow, to = entry.reply_to, value = res })
      else
        __send({ kind = "error", flow = entry.flow, to = entry.reply_to, err = tostring(res) })
      end
    elseif not ok then
      print("!! unhandled flow error: " .. tostring(res))
    end
  else
    assert(ok, "segment suspended abnormally: " .. tostring(res))
    local st = stacks[entry.flow] or {}
    st[#st + 1] = entry
    stacks[entry.flow] = st
  end
end

-- Start a flow on this side (the compiler wraps event handlers in this).
function rt.start_flow(fn, ...)
  next_flow = next_flow + 1
  local flow = my_addr() .. "#" .. next_flow
  local co = coroutine.create(fn)
  contexts[co] = { flow = flow, origin = SIDE == "browser" and SESSION or nil }
  step({ co = co, flow = flow, on_complete = "done" }, ...)
end

-- Transport delivery point.
function rt.receive(pkt)
  if pkt.kind == "call" or pkt.kind == "cast" then
    local fn = hops[pkt.hop]
    if not fn then
      if pkt.kind == "call" then
        __send({ kind = "error", flow = pkt.flow, to = pkt.reply_to,
                 err = "unknown hop: " .. tostring(pkt.hop) })
      end
      return
    end
    local co = coroutine.create(fn)
    contexts[co] = { flow = pkt.flow, origin = pkt.origin }
    step({ co = co, flow = pkt.flow,
           on_complete = pkt.kind == "call" and "reply" or "done",
           reply_to = pkt.reply_to }, pkt.vars)
  else
    -- reply/error resumes the most recently suspended coroutine of the
    -- flow on this side — LIFO, because hops nest like calls.
    local st = stacks[pkt.flow]
    assert(st and #st > 0, "reply for a flow with nothing suspended: " .. pkt.flow)
    local entry = table.remove(st)
    if #st == 0 then stacks[pkt.flow] = nil end
    if pkt.kind == "reply" then
      step(entry, true, pkt.value)
    else
      step(entry, false, pkt.err)
    end
  end
end

-- True when nothing on this side is suspended waiting for a reply. The
-- host checks this after draining the queue: a leaked flow means a reply
-- was lost or misrouted.
function rt.quiescent()
  for flow, st in pairs(stacks) do
    if #st > 0 then
      return false, flow
    end
  end
  return true
end

function __receive(pkt)
  rt.receive(pkt)
end

-- Entry-point helpers for hosts and browser glue -----------------------------

-- The browser glue fires DOM events through this: each event is one flow.
function __fire(name, arg)
  rt.start_flow(function()
    _G[name](arg)
  end)
end

-- hopd calls this on the server VM when a tab connects. The app may define
-- on_connect(sid) to bring the newcomer up to date. Server-origin flow:
-- browser!() is not available inside it, casts are.
function __session_connect(sid)
  if _G.on_connect ~= nil then
    rt.start_flow(function()
      on_connect(sid)
    end)
  end
end
