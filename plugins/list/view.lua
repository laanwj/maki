-- Brain-side view for the executor's list tool: the listing only exists once
-- the call ends, so done fills the live buf from the output text.

local dir_listing = require("maki.dir_listing")
local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

maki.api.register_tool_view({
  tool = "list",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = dir_listing.opts(ctx) }, shorten_path(input.path or "")
  end,

  done = function(state, _, output, _)
    ToolView.populate(state.buf, output, state.opts)
  end,
})
