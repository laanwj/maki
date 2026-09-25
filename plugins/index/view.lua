-- Brain-side view for the executor's index tool: the skeleton only exists
-- once the call ends, so done renders it into the live buf. A directory
-- answer is a plain name list, which the skeleton render passes through
-- unmarked — the same lines the single-process listing view shows.

local index_view = require("index_view")

maki.api.register_tool_view({
  tool = "index",

  start = function(input, ctx)
    local buf = maki.ui.buf()
    ctx:live_buf(buf)
    return { buf = buf, opts = index_view.view_opts(ctx) }, index_view.summary(input)
  end,

  done = function(state, input, output, is_error)
    if is_error then
      state.buf:set_lines({ output })
      return
    end
    local ext = (input.path or ""):match("%.([^%.]+)$") or ""
    index_view.render_into(state.buf, output, state.opts, ext)
  end,
})
