-- Brain-side views for the executor's edit family: the diff paints when the
-- call starts (the input is the whole change), and done only swaps in the
-- error text when the edit failed.

local edit_view = require("edit_view")

local function register(name, blocks_from)
  maki.api.register_tool_view({
    tool = name,

    start = function(input, ctx)
      local buf = edit_view.diff_view(blocks_from(input), input.path)
      ctx:live_buf(buf)
      return { buf = buf }, edit_view.summary(input)
    end,

    done = function(state, _, output, is_error)
      if is_error then
        state.buf:set_lines({ output })
      end
    end,
  })
end

register("edit", edit_view.blocks_edit)
register("multiedit", edit_view.blocks_multiedit)
register("edit_lines", edit_view.blocks_edit_lines)
register("insert_lines", edit_view.blocks_insert_lines)
