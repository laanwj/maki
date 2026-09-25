-- Brain-side view for the executor's write tool: the preview paints when the
-- call starts (the input carries the whole content), and done only swaps in
-- the error text when the write failed.

local write_view = require("write_view")

maki.api.register_tool_view({
  tool = "write",

  start = function(input, ctx)
    local buf = write_view.build_view(input.content or "", input.path or "", ctx)
    ctx:live_buf(buf)
    return { buf = buf }, write_view.summary(input)
  end,

  done = function(state, _, output, is_error)
    if is_error then
      state.buf:set_lines({ output })
    end
  end,
})
