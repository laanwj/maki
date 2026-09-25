-- The glob tool's display, shared between the plugin (single-process) and
-- the brain-side view (split mode).

local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

local M = {}

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 3, keep = "head" }
end

function M.build_from_lines(lines, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, M.view_opts(ctx))
  for _, line in ipairs(lines) do
    view:append(line)
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

function M.summary(input)
  local s = shorten_path(input.pattern or "")
  if input.path then
    s = s .. " in " .. shorten_path(input.path)
  end
  return s
end

return M
