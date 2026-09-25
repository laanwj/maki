-- The index tool's display, shared between the plugin (single-process) and
-- the brain-side view (split mode), which renders the skeleton text the
-- call returned.

local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")
local indexer = require("indexer")

local M = {}

local TRUNCATED_SUFFIX = indexer.TRUNCATED_SUFFIX
local TRUNCATED_INFIX = " more truncated]"

local function split_trailing_range(line)
  local pos = line:find(" %[%d[%d%-,]*%]$")
  if not pos then
    return nil, nil
  end
  return line:sub(1, pos - 1), line:sub(pos + 1)
end

local function is_section_header(line)
  if line:sub(1, 1) == " " then
    return false
  end
  local trimmed = line:match("^(.-)%s*$")
  if trimmed:sub(-1) == ":" then
    return true
  end
  local body = split_trailing_range(trimmed)
  return body ~= nil and body:sub(-1) == ":"
end

local function infer_line_meta(line)
  if line == "" then
    return nil
  end
  if is_section_header(line) then
    local body, range = split_trailing_range(line)
    if range then
      return { tag = "section", body = body .. " ", range = range }
    end
    return { tag = "section" }
  end
  if
    line:sub(-#TRUNCATED_SUFFIX) == TRUNCATED_SUFFIX
    or (line:match("^%s*%[") and line:sub(-#TRUNCATED_INFIX) == TRUNCATED_INFIX)
  then
    return { tag = "dim" }
  end
  local body, range = split_trailing_range(line)
  if range then
    return { body = body, range = range }
  end
  return nil
end

local function render_skeleton(view, text, meta)
  local hl_entries = {}
  for line_nr, line in ipairs(maki.split(text:gsub("\n+$", ""), "\n")) do
    local m = (meta and meta[line_nr]) or infer_line_meta(line)
    if line == "" then
      view:append("")
    elseif m and m.tag == "section" then
      if m.range then
        view:append({ { m.body, "section" }, { m.range, "line_nr" } })
      else
        view:append({ { line, "section" } })
      end
    elseif m and m.tag == "dim" then
      view:append({ { line, "dim" } })
    elseif m and m.range then
      view:append({ { m.body }, { " " }, { m.range, "line_nr" } })
      hl_entries[#hl_entries + 1] = { idx = #view.all_lines, text = m.body, range = m.range }
    else
      view:append({ { line } })
      hl_entries[#hl_entries + 1] = { idx = #view.all_lines, text = line }
    end
  end
  return hl_entries
end

local function apply_highlights(view, hl_entries, ext)
  if #hl_entries == 0 then
    return
  end
  local texts = {}
  for _, e in ipairs(hl_entries) do
    texts[#texts + 1] = e.text
  end
  local highlighted = maki.ui.highlight(table.concat(texts, "\n"), ext, { independent = true })
  if not highlighted then
    return
  end
  for i, e in ipairs(hl_entries) do
    local hl_spans = highlighted[i]
    if hl_spans then
      local new_line = {}
      for _, span in ipairs(hl_spans) do
        new_line[#new_line + 1] = span
      end
      if e.range then
        new_line[#new_line + 1] = { " " }
        new_line[#new_line + 1] = { e.range, "line_nr" }
      end
      view:update_line(e.idx, new_line)
    end
  end
  view:flush()
end

function M.render_header(path, line_count)
  local buf = maki.ui.buf()
  local spans = { { shorten_path(path), "path" } }
  if line_count then
    spans[#spans + 1] = { " (" .. line_count .. " lines)", "dim" }
  end
  buf:line(spans)
  return buf
end

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.index) or 5, keep = "head" }
end

-- Renders into an existing buf: the brain-side view's done fills the live
-- buf its start published.
function M.render_into(buf, skeleton, opts, ext, line_meta)
  local view = ToolView.new(buf, opts)
  buf:on("click", function()
    view:toggle()
  end)
  local hl_entries = render_skeleton(view, skeleton, line_meta)
  view:finish()

  if ext then
    maki.async.run(function()
      apply_highlights(view, hl_entries, ext)
    end)
  end
end

function M.render_index(skeleton, path, ctx, ext, line_meta)
  local buf = maki.ui.buf()
  M.render_into(buf, skeleton, M.view_opts(ctx), ext, line_meta)
  local line_count = select(2, skeleton:gsub("\n", "\n")) + 1
  return buf, M.render_header(path, line_count)
end

function M.summary(input)
  return shorten_path(input.path or "")
end

return M
