-- Brain-side view for the executor's grep tool: the matches only exist once
-- the call ends, so done rebuilds the view from the output text.

local grep_view = require("grep_view")

maki.api.register_tool_view({
  tool = "grep",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = grep_view.view_opts(ctx) }, grep_view.summary(input)
  end,

  done = function(state, _, output, is_error)
    if is_error then
      state.buf:set_lines({ output })
      return
    end
    local entries = grep_view.parse_llm_output(output)
    if #entries == 0 then
      state.buf:set_lines({ output })
      return
    end
    grep_view.populate(state.buf, entries, state.opts)
  end,
})
