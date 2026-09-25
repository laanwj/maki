-- Brain-side view for the executor's glob tool: the result list only exists
-- once the call ends, so done fills the live buf from the output text.

local glob_view = require("glob_view")
local ToolView = require("maki.tool_view")

maki.api.register_tool_view({
  tool = "glob",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = glob_view.view_opts(ctx) }, glob_view.summary(input)
  end,

  done = function(state, _, output, _)
    ToolView.populate(state.buf, output, state.opts)
  end,
})
