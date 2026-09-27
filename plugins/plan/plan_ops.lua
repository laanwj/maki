local fuzzy_replace = require("maki.fuzzy_replace")

local M = {}

function M.write_plan(path, content)
  local parent = maki.fs.dirname(path)
  if parent then
    maki.fs.mkdir(parent, { parents = true })
  end
  local _, err = maki.fs.write(path, content)
  if err then
    return nil, "write error: " .. tostring(err)
  end
  return true
end

--- Returns the content, or nil + "missing" when no plan file exists yet.
function M.read_plan(path)
  local exists = maki.fs.metadata(path) ~= nil
  if not exists then
    return nil, "missing"
  end
  local content, err = maki.fs.read(path)
  if not content then
    return nil, "read error: " .. tostring(err)
  end
  return content
end

--- Returns before/after, or nil + err.
function M.edit_plan(path, old_string, new_string, replace_all)
  local before, read_err = M.read_plan(path)
  if not before then
    if read_err == "missing" then
      return nil, "no plan file yet; create it with plan_write"
    end
    return nil, read_err
  end
  local after, replace_err = fuzzy_replace.replace(before, old_string, new_string, replace_all or false)
  if not after then
    return nil, replace_err
  end
  local _, write_err = maki.fs.write(path, after)
  if write_err then
    return nil, "write error: " .. tostring(write_err)
  end
  return before, after
end

return M
