-- hui.lua — hiccup UI for hop apps. Loaded into every VM after hoprt.lua.
--
-- A node is data:  [:li, { class = "done", onclick = fn }, "buy milk"]
--   node[1]  tag (string)
--   node[2]  attrs table (optional; detected by not being a node/string)
--   rest     children: strings, numbers, nodes, or lists of nodes
-- A table whose [1] is not a string is a fragment: children spliced in
-- place, which is what a list built with table.insert renders as.
--
-- Function-valued attributes are event handlers. They are NOT serialized:
-- the closure stays in this VM's handler table and the rendered HTML calls
-- back into it by id (__hopHandler in the browser glue). Handlers run as
-- flows, so a handler body may hop: onclick = fn(e) { server!(); ... }.

hui = {}

local handlers = {}      -- id -> closure
local handler_seq = 0
local root_ids = {}      -- selector -> ids minted by its last render

local function esc(s)
  s = tostring(s)
  s = string.gsub(s, "&", "&amp;")
  s = string.gsub(s, "<", "&lt;")
  s = string.gsub(s, ">", "&gt;")
  s = string.gsub(s, "\"", "&quot;")
  return s
end

local function render_node(node, ids)
  if type(node) ~= "table" then
    return esc(node)
  end
  if type(node[1]) ~= "string" then
    -- fragment: a list of nodes (or empty)
    local html = ""
    for _, child in ipairs(node) do
      html = html .. render_node(child, ids)
    end
    return html
  end

  local tag = node[1]
  local attrs = ""
  local first_child = 2
  local a = node[2]
  if type(a) == "table" and type(a[1]) ~= "string" and #a == 0 then
    -- an attrs map (possibly empty), not a child node/fragment
    first_child = 3
    local keys = {}
    for k in pairs(a) do
      keys[#keys + 1] = k
    end
    table.sort(keys) -- deterministic HTML: same tree, same string
    for _, k in ipairs(keys) do
      local v = a[k]
      if type(v) == "function" then
        handler_seq = handler_seq + 1
        handlers[handler_seq] = v
        ids[#ids + 1] = handler_seq
        attrs = attrs .. " " .. k .. "=\"__hopHandler(" .. handler_seq .. ")\""
      elseif v == true then
        attrs = attrs .. " " .. k
      elseif v ~= false then
        attrs = attrs .. " " .. k .. "=\"" .. esc(v) .. "\""
      end
    end
  end

  local html = "<" .. tag .. attrs .. ">"
  for i = first_child, #node do
    html = html .. render_node(node[i], ids)
  end
  return html .. "</" .. tag .. ">"
end

-- Render a node tree into a DOM selector. Handlers minted by this root's
-- previous render are released; ids are never reused within a session.
function hui.render(sel, node)
  for _, id in ipairs(root_ids[sel] or {}) do
    handlers[id] = nil
  end
  local ids = {}
  root_ids[sel] = ids
  dom.set(sel, render_node(node, ids))
end

-- The browser glue routes __hopHandler(id) from rendered HTML to here.
-- Each activation is one flow — handler bodies may hop.
function __handler_fire(id)
  local h = handlers[id]
  if h ~= nil then
    rt.start_flow(function()
      h(nil)
    end)
  end
end
