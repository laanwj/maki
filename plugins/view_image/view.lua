-- Brain-side view for the executor's view_image tool: the caption is the
-- whole display (the image itself goes to the model, never to the screen).

local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

maki.api.register_tool_view({
  tool = "view_image",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    local header = maki.ui.buf({ scratch = true })
    header:line({ { shorten_path(input.path or ""), "path" } })
    return { buf = buf }, header
  end,

  done = function(state, _, output, _)
    ToolView.populate(state.buf, output, { max_lines = 10, keep = "head" })
  end,
})
