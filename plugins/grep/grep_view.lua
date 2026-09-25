-- The grep tool's display, shared between the plugin (single-process) and
-- the brain-side view (split mode), which rebuilds it from the output text
-- via parse_llm_output.

local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")
local color = require("maki.color")

local M = {}

local DIM_FACTOR = 0.3

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.other) or 10, keep = "head" }
end

local function has_context(groups)
  for _, group in ipairs(groups) do
    if #group.lines > 1 then
      return true
    end
  end
  return false
end

local function dim_spans(spans)
  local result = {}
  for _, span in ipairs(spans) do
    local style = span[2]
    local fg = type(style) == "table" and style.fg or nil
    local faded = fg and color.dim(fg, DIM_FACTOR)
    -- A palette color has no value to blend, so the terminal dims it instead.
    result[#result + 1] = faded and { span[1], { fg = faded } } or { span[1], { fg = fg, dim = true } }
  end
  return result
end

local function apply_grep_highlights(hl_tasks, view)
  for _, task in ipairs(hl_tasks) do
    local texts = {}
    for _, fl in ipairs(task.lines) do
      texts[#texts + 1] = fl.text
    end

    local highlighted = maki.ui.highlight(table.concat(texts, "\n"), task.ext, { independent = true })
    if highlighted then
      for i, fl in ipairs(task.lines) do
        local hl_spans = highlighted[i]
        if hl_spans then
          local nr_span = view.all_lines[fl.idx][1]
          local spans = fl.is_match and hl_spans or dim_spans(hl_spans)
          view:update_line(fl.idx, { nr_span, table.unpack(spans) })
        end
      end
    end
  end
end

-- Populates an existing buf: the brain-side view's done fills the live buf
-- its start published.
function M.populate(buf, entries, opts)
  local view = ToolView.new(buf, opts)

  local max_nr = 0
  for _, entry in ipairs(entries) do
    for _, group in ipairs(entry.groups) do
      for _, line in ipairs(group.lines) do
        if line.line_nr > max_nr then
          max_nr = line.line_nr
        end
      end
    end
  end
  local nr_fmt = ToolView.line_nr_fmt(max_nr) .. " "

  local hl_tasks = {}

  for _, entry in ipairs(entries) do
    if #entries > 1 then
      view:append({ { shorten_path(entry.path), "path" } })
    end

    local ctx_lines = has_context(entry.groups)
    local file_lines = {}

    for gi, group in ipairs(entry.groups) do
      if gi > 1 and ctx_lines then
        view:append({ { "  --", "dim" } })
      end
      for _, line in ipairs(group.lines) do
        view:append({ { string.format(nr_fmt, line.line_nr), "line_nr" }, { line.text } })
        file_lines[#file_lines + 1] = {
          idx = #view.all_lines,
          text = line.text,
          is_match = line.is_match,
        }
      end
    end

    hl_tasks[#hl_tasks + 1] = {
      ext = entry.path:match("%.([^%.]+)$") or "",
      lines = file_lines,
    }
  end

  view:finish()

  apply_grep_highlights(hl_tasks, view)
  view:flush()

  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

function M.build(entries, ctx)
  return M.populate(maki.ui.buf(), entries, M.view_opts(ctx))
end

function M.parse_llm_output(text)
  local entries = {}
  local current
  for _, line in ipairs(maki.split(text, "\n")) do
    local path = line:match("^(%S.+):$")
    if path then
      current = { path = path, groups = { { lines = {} } } }
      entries[#entries + 1] = current
    elseif current then
      if line == "  --" then
        current.groups[#current.groups + 1] = { lines = {} }
      else
        local nr, sep, content = line:match("^%s+(%d+)([:]) (.*)$")
        if not nr then
          nr, sep, content = line:match("^%s+(%d+)( ) (.*)$")
        end
        if nr then
          local group = current.groups[#current.groups]
          group.lines[#group.lines + 1] = {
            line_nr = tonumber(nr),
            text = content or "",
            is_match = sep == ":",
          }
        end
      end
    end
  end
  return entries
end

function M.summary(input)
  local s = (input.pattern or ""):gsub('"$', "")
  if input.include then
    s = s .. " [" .. input.include .. "]"
  end
  if input.path then
    s = s .. " " .. shorten_path(input.path)
  end
  return s
end

return M
