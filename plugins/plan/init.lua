local plan_ops = require("plan_ops")
local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

local PLAN_READ_DESCRIPTION = [[Read the session's plan file. Works in any mode, as long as the session has a plan.]]

local PLAN_WRITE_DESCRIPTION = [[Write the session's plan file, replacing existing content.

- The plan file's path is fixed by the session; this works in any mode.
- Use plan_edit for targeted changes to an existing plan.]]

local PLAN_EDIT_DESCRIPTION = [[Replace an exact string match in the session's plan file.

- Works in any mode, as long as the session has a plan.
- The old_string must appear exactly once unless replace_all is true.
- plan_read first to get exact content.]]

local DIFF_OLD = { style = "diff_old", sign = "diff_old_sign" }
local DIFF_NEW = { style = "diff_new", sign = "diff_new_sign" }

local function view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.write) or 10, keep = "head" }
end

local function content_view(content, ctx)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, view_opts(ctx))
  view:set_highlight(content, "md")
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

local function split_lines(text)
  local lines = maki.split(text, "\n")
  if lines[#lines] == "" then
    lines[#lines] = nil
  end
  return lines
end

local function diff_view(old_text, new_text)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = math.huge, keep = "head" })
  for _, line in ipairs(split_lines(old_text)) do
    view:append({ { "- ", DIFF_OLD.sign }, { line, DIFF_OLD.style } })
  end
  for _, line in ipairs(split_lines(new_text)) do
    view:append({ { "+ ", DIFF_NEW.sign }, { line, DIFF_NEW.style } })
  end
  view:finish()
  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

-- Parallel calls in one batch race the plan file's read-modify-write (the
-- path comes from the session, not the input, so the dispatch file lock
-- never sees these). One writer at a time.
local plan_lock = maki.async.semaphore(1)

-- pcall so a raised error cannot leak the permit.
local function with_plan_lock(fn, input, ctx)
  local permit = plan_lock:acquire()
  local ok, reply = pcall(fn, input, ctx)
  permit:release()
  if not ok then
    error(reply, 0)
  end
  return reply
end

local function plan_path(ctx, tool)
  local path, err = ctx:plan_path()
  if err then
    return nil, err
  end
  if not path then
    return nil, tool .. " needs a plan file, but this session has none (enter plan mode first)"
  end
  return path
end

maki.api.register_tool({
  name = "plan_read",
  kind = "read",
  audiences = { "main" },
  description = PLAN_READ_DESCRIPTION,

  schema = { type = "object", properties = {} },

  restore = function(_, output, _, ctx)
    return ToolView.restore(output, view_opts(ctx))
  end,

  handler = function(_, ctx)
    local path, err = plan_path(ctx, "plan_read")
    if not path then
      return { llm_output = err, is_error = true }
    end
    local content, read_err = plan_ops.read_plan(path)
    if not content then
      if read_err == "missing" then
        return { llm_output = "(no plan written yet)" }
      end
      return { llm_output = read_err, is_error = true }
    end
    return { llm_output = content, body = content_view(content, ctx) }
  end,
})

local function write_plan_reply(input, ctx)
  local path, err = plan_path(ctx, "plan_write")
  if not path then
    return { llm_output = err, is_error = true }
  end
  local content = input.content
  if not content then
    return { llm_output = "error: content is required", is_error = true }
  end
  local _, write_err = plan_ops.write_plan(path, content)
  if write_err then
    return { llm_output = write_err, is_error = true }
  end
  return {
    llm_output = string.format("wrote %d bytes to plan %s", #content, shorten_path(path)),
    body = content_view(content, ctx),
    annotation = string.format("%d bytes", #content),
    written_path = path,
  }
end

local function edit_plan_reply(input, ctx)
  local path, err = plan_path(ctx, "plan_edit")
  if not path then
    return { llm_output = err, is_error = true }
  end
  if not input.old_string or not input.new_string then
    return { llm_output = "error: old_string and new_string are required", is_error = true }
  end
  local before, after = plan_ops.edit_plan(path, input.old_string, input.new_string, input.replace_all)
  if not before then
    return { llm_output = after, is_error = true }
  end
  return {
    llm_output = "edited plan " .. shorten_path(path),
    body = diff_view(input.old_string, input.new_string),
    written_path = path,
  }
end

maki.api.register_tool({
  name = "plan_write",
  kind = "edit",
  audiences = { "main" },
  description = PLAN_WRITE_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      content = {
        type = "string",
        description = "The complete plan content",
        required = true,
      },
    },
  },

  restore = function(input, output, is_error, ctx)
    if is_error or not input.content then
      return ToolView.restore(output, view_opts(ctx))
    end
    return content_view(input.content, ctx)
  end,

  handler = function(input, ctx)
    return with_plan_lock(write_plan_reply, input, ctx)
  end,
})

maki.api.register_tool({
  name = "plan_edit",
  kind = "edit",
  audiences = { "main" },
  description = PLAN_EDIT_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      old_string = {
        type = "string",
        description = "Exact string to find (must match uniquely unless replace_all is true)",
        required = true,
      },
      new_string = {
        type = "string",
        description = "Replacement string",
        required = true,
      },
      replace_all = {
        type = "boolean",
        description = "Replace all occurrences (default false)",
      },
    },
  },

  restore = function(input, output, is_error, ctx)
    if is_error or not input.old_string then
      return ToolView.restore(output, view_opts(ctx))
    end
    return diff_view(input.old_string, input.new_string or "")
  end,

  handler = function(input, ctx)
    return with_plan_lock(edit_plan_reply, input, ctx)
  end,
})
