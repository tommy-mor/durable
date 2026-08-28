-- store.lua — schema constructors and query helpers for hop apps.
-- Loaded into the server VM only, before the app chunk. Rust bind() then
-- hangs append / one / entries / … off this same table.
--
-- A schema is data, field order is the array order of store.record:
--   store.record({ { "text", store.leaf }, { "done", store.leaf } })

store = store or {}

store.leaf = { k = "leaf" }
store.sum = { k = "sum" }

function store.map(of)
  return { k = "map", of = of }
end

function store.list(of)
  return { k = "list", of = of }
end

function store.deque(of)
  return { k = "deque", of = of }
end

function store.record(fields)
  return { k = "record", fields = fields }
end

-- Map → array of {id=key, ...fields} so hop `for` (ipairs) can render it.
function store.items(path)
  local es = store.entries(path)
  local out = {}
  for i, pair in ipairs(es) do
    local rec = pair[2]
    if type(rec) == "table" then
      rec.id = pair[1]
      out[i] = rec
    else
      out[i] = { id = pair[1], value = rec }
    end
  end
  return out
end
