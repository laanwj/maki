-- Brain-side view for the executor's skill tool. Native and remote views
-- share the parser and highlighted renderer.

local ToolView = require("maki.tool_view")
local skill_view = require("skill_view")

maki.api.register_tool_view({
  tool = "skill",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = skill_view.view_opts(ctx) }, input.name
  end,

  done = function(state, _, output, _)
    if not skill_view.populate_output(state.buf, output, state.opts) then
      ToolView.populate(state.buf, output, state.opts)
    end
  end,
})
