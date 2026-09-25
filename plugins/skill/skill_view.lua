local ToolView = require("maki.tool_view")
local helpers = require("skill_helpers")

local M = {}

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 20, keep = "head" }
end

function M.populate_output(buf, output, opts)
  local location, content = helpers.parse_output(output)
  if not location then
    return nil
  end

  local view = ToolView.new(buf, opts)
  buf:on("click", function()
    view:toggle()
  end)
  view:set_highlight(content, location:match("%.([^%.]+)$") or "md")
  view:finish()
  return location
end

return M
