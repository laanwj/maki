-- The write tool's display, shared between the plugin (single-process, where
-- the tool renders itself) and the brain-side view (split mode, where the
-- executor only executes and this paints what it sent).

local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

local M = {}

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.write) or 10, keep = "head" }
end

function M.build_view(content, path, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, M.view_opts(ctx))
  view:set_highlight(content, path:match("%.([^%.]+)$") or "")
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

function M.summary(input)
  return shorten_path(input.path or "")
end

return M
