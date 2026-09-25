-- Brain-side view for the executor's read tool: the body is the file
-- content, which only exists once the call ends, so start publishes an empty
-- live buf and done fills it from the output text.

local read_view = require("read_view")

maki.api.register_tool_view({
  tool = "read",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = read_view.view_opts(ctx) }, read_view.summary(input)
  end,

  done = function(state, input, output, is_error)
    if is_error then
      state.buf:set_lines({ output })
      return
    end
    local lines, start_line, total_lines = read_view.parse_output(output)
    if #lines == 0 then
      state.buf:set_lines({ output })
      return
    end
    start_line = start_line or 1
    total_lines = total_lines or (start_line + #lines - 1)
    read_view.populate(state.buf, lines, start_line, total_lines, input.path or "", state.opts)
  end,
})
