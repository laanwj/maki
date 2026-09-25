-- Brain-side view for the executor's skill tool. The single-process restore
-- renders the same plain text the output carries, so done matches it.

local ToolView = require("maki.tool_view")

local function view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 20, keep = "head" }
end

maki.api.register_tool_view({
  tool = "skill",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = view_opts(ctx) }, input.name
  end,

  done = function(state, _, output, _)
    ToolView.populate(state.buf, output, state.opts)
  end,
})
