-- The read tool's display, shared between the plugin (single-process, where
-- the tool renders itself) and the brain-side view (split mode, where the
-- output text is the whole record that crosses the wire).

local ToolView = require("maki.tool_view")
local shorten_path = require("maki.shorten_path")

local M = {}

function M.view_opts(ctx)
  local tol = ctx:tool_output_lines()
  return { max_lines = (tol and tol.read) or 10, keep = "head" }
end

local function apply_highlights(view, lines, ext, prefix)
  local opts = prefix and { prefix = prefix } or nil
  local highlighted = maki.ui.highlight(table.concat(lines, "\n"), ext, opts)
  if not highlighted then
    return
  end
  for i, hl_spans in ipairs(highlighted) do
    local plain = view.all_lines[i]
    if not plain then
      break
    end
    view:update_line(i, { plain[1], table.unpack(hl_spans) })
  end
  view:flush()
end

-- Populates an existing buf: the brain-side view builds its body into the
-- live buf `start` published, since `done` gets no ctx to publish one with.
function M.populate(buf, lines, start_line, total_lines, path, opts, prefix)
  local view = ToolView.new(buf, opts)
  local nr_fmt = ToolView.line_nr_fmt(start_line + #lines - 1) .. " "

  for i, line in ipairs(lines) do
    view:append({ { string.format(nr_fmt, start_line + i - 1), "line_nr" }, { line } })
  end

  local trunc_start = start_line + #lines
  if trunc_start <= total_lines then
    view:append({
      {
        string.format(
          "... Truncated %d lines. Use offset=%d to read further.",
          total_lines - trunc_start + 1,
          trunc_start
        ),
        "dim",
      },
    })
  end

  view:finish()

  local ext = path:match("%.([^%.]+)$") or ""
  maki.async.run(function()
    apply_highlights(view, lines, ext, prefix)
  end)

  buf:on("click", function()
    view:toggle()
  end)
  return buf
end

function M.build_file_view(lines, start_line, total_lines, path, ctx, prefix)
  return M.populate(maki.ui.buf(), lines, start_line, total_lines, path, M.view_opts(ctx), prefix)
end

-- Numbered lines and the truncation marker parse back out of the output text.
function M.parse_output(output)
  local lines, start_line, total_lines = {}, nil, nil
  for _, raw in ipairs(maki.split(output, "\n")) do
    local nr, text = raw:match("^%s*(%d+): (.*)$")
    if nr then
      start_line = start_line or tonumber(nr)
      lines[#lines + 1] = text
    else
      local trunc_end = raw:match("Truncated lines: %d+%-(%d+)")
      if trunc_end then
        total_lines = tonumber(trunc_end)
      end
    end
  end
  return lines, start_line, total_lines
end

function M.render_output(input, output, ctx)
  local lines, start_line, total_lines = M.parse_output(output)
  if #lines == 0 then
    return ToolView.restore(output, M.view_opts(ctx))
  end
  start_line = start_line or 1
  total_lines = total_lines or (start_line + #lines - 1)
  return M.build_file_view(lines, start_line, total_lines, input.path or "", ctx)
end

function M.summary(input)
  local s = shorten_path(input.path or "")
  local start = input.offset or 1
  if input.limit and input.limit > 0 then
    s = s .. ":" .. start .. "-" .. (start + input.limit - 1)
  else
    s = s .. ":" .. start
  end
  return s
end

return M
