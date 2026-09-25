-- The edit family's display: diff rendering shared between the plugin
-- (single-process, where the tool renders itself) and the brain-side view
-- (split mode, where the executor only executes and this paints the call).

local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

local M = {}

local DIFF_OLD = { style = "diff_old", prefix = "- ", sign = "diff_old_sign", nr = "diff_old_line_nr" }
local DIFF_NEW = { style = "diff_new", prefix = "+ ", sign = "diff_new_sign", nr = "diff_new_line_nr" }

local FALLBACK_VIEW_LINES = 10

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.write) or FALLBACK_VIEW_LINES, keep = "head" }
end

local function split_lines(text)
  local lines = maki.split(text, "\n")
  if lines[#lines] == "" then
    lines[#lines] = nil
  end
  return lines
end

-- Line number of `needle` in `content` (plain find, first match).
local function line_of(content, needle)
  local pos = content:find(needle, 1, true)
  if not pos then
    return nil
  end
  local _, newlines = content:sub(1, pos - 1):gsub("\n", "")
  return newlines + 1
end

-- The edit already happened, so each block's `new` text sits in the file
-- right now: read it once and recover real line numbers. Best effort,
-- blocks stay unnumbered when the text moved or the file is gone — which
-- brain-side in split mode is always, the file being the executor's.
local function resolve_block_nrs(blocks, path)
  if not path then
    return
  end
  local content
  for _, b in ipairs(blocks) do
    if not b.nr and (b.new or "") ~= "" then
      content = content or maki.fs.read(maki.fs.abspath(path))
      if not content then
        return
      end
      b.nr = line_of(content, b.new)
    end
  end
end

local function gutter_width(blocks)
  local max_nr = 0
  for _, b in ipairs(blocks) do
    if b.nr then
      local n = #split_lines(b.old or "")
      if n == 0 then
        n = #split_lines(b.new or "")
      end
      max_nr = math.max(max_nr, b.nr + n - 1)
    end
  end
  return max_nr > 0 and #tostring(max_nr) or 0
end

-- The one gutter builder both render passes share: the plain render and
-- the async highlight rewrite must produce byte-identical gutters or the
-- columns shift when highlights land.
local function nr_span(fmt, start_nr, i, style)
  return { string.format(fmt, start_nr and (start_nr + i - 1) or ""), style }
end

local function append_diff_lines(view, text, side, nr_fmt, start_nr, jobs)
  local lines = split_lines(text or "")
  if #lines == 0 then
    return
  end
  jobs[#jobs + 1] = {
    first = #view.all_lines + 1,
    text = table.concat(lines, "\n"),
    side = side,
    start_nr = start_nr,
  }
  for i, line in ipairs(lines) do
    local spans = {}
    if nr_fmt then
      spans[#spans + 1] = nr_span(nr_fmt, start_nr, i, side.nr)
    end
    spans[#spans + 1] = { side.prefix, side.sign }
    spans[#spans + 1] = { line, side.style }
    view:append(spans)
  end
end

-- Re-renders the block's lines with syntax colors on the diff backgrounds,
-- keeping the gutter and prefix the plain render put there.
local function apply_highlights(view, fmt, jobs, ext)
  maki.async.run(function()
    for _, job in ipairs(jobs) do
      local side = maki.ui.theme_style(job.side.style)
      local bg = side and side.bg
      local highlighted = bg and maki.ui.highlight(job.text, ext)
      for i, hl_line in ipairs(highlighted or {}) do
        local idx = job.first + i - 1
        if not view.all_lines[idx] then
          break
        end
        local spans = {}
        if fmt then
          spans[#spans + 1] = nr_span(fmt, job.start_nr, i, job.side.nr)
        end
        spans[#spans + 1] = { job.side.prefix, job.side.sign }
        for _, seg in ipairs(hl_line) do
          local s = type(seg[2]) == "table" and seg[2] or {}
          s.bg = bg
          spans[#spans + 1] = { seg[1], s }
        end
        view:update_line(idx, spans)
      end
    end
    view:flush()
  end)
end

-- Mirrors the standalone Rust diff render (code_view.rs): numbered gutter
-- on removed lines, blank gutter + `+` on added lines, and no truncation
-- ever, a diff is exactly the change and hiding part of it lies.
function M.diff_view(blocks, path)
  local buf = maki.ui.buf()
  local view = ToolView.new(buf, { max_lines = math.huge, keep = "head" })
  resolve_block_nrs(blocks, path)
  local w = gutter_width(blocks)
  local fmt = w > 0 and ("%" .. w .. "s ") or nil
  local jobs = {}
  local function append(text, side, start_nr)
    append_diff_lines(view, text, side, fmt, start_nr, jobs)
  end
  for i, block in ipairs(blocks) do
    if i > 1 then
      view:append({})
    end
    local has_old = (block.old or "") ~= ""
    append(block.old, DIFF_OLD, block.nr)
    append(block.new, DIFF_NEW, not has_old and block.nr or nil)
  end
  view:finish()
  local ext = (path or ""):match("%.([^%.]+)$")
  if #jobs > 0 and ext then
    apply_highlights(view, fmt, jobs, ext)
  end
  return buf
end

function M.diff_restore(blocks_from)
  return function(input, output, is_error, ctx)
    if is_error then
      return ToolView.restore(output, M.view_opts(ctx))
    end
    return M.diff_view(blocks_from(input), input.path)
  end
end

function M.summary(input)
  return shorten_path(input.path or "")
end

-- The per-tool input shapes, shared by the plugin's restores and the view.
function M.blocks_edit(input)
  return { { old = input.old_string, new = input.new_string } }
end

function M.blocks_multiedit(input)
  local blocks = {}
  for _, edit in ipairs(input.edits or {}) do
    blocks[#blocks + 1] = { old = edit.old_string, new = edit.new_string }
  end
  return blocks
end

function M.blocks_edit_lines(input)
  return { { new = input.new_string, nr = input.start } }
end

function M.blocks_insert_lines(input)
  return { { new = input.new_string, nr = (input.line or 0) + 1 } }
end

return M
