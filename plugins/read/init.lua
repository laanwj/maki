local read_view = require("read_view")
local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")
local helpers = require("read_helpers")

local truncate_bytes = helpers.truncate_bytes
local split_lines = helpers.split_lines
local build_file_view = read_view.build_file_view

local DESCRIPTION = [[Read a file. Returns contents with line numbers (1-indexed).

- Supports absolute, relative, and ~/ paths.
- **offset** and **limit** are required. Use offset=1 to read from the first line.
- Use limit=0 to read until the end of file (capped at 2000 lines).
- Use the **index** tool or **grep** tool first to find the offset and limit.
- Only read the sections you actually need.
- Use `wc -l` to check total number of lines before reading to decide a reasonable limit.
- Use truncation hints (e.g. "truncated lines X-Y") to continue with the correct offset.
- Do not reread the same range (same file and same offset).
- Prefer grep to locate content instead of scanning full files.
- Call in parallel when reading multiple files.
- Avoid tiny repeated slices - read a larger window if you need more context.]]

local DEFAULT_MAX_OUTPUT_LINES = 2000

local opts = maki.api.register_options({
  max_line_bytes = { default = 500, min = 80, desc = "Truncate lines longer than this many bytes." },
  max_output_lines = output_limits.specs.max_output_lines,
})

local function read_file(path, offset, limit, ctx)
  local content, err = maki.fs.read(path)
  if not content then
    return { llm_output = "read error: " .. tostring(err), is_error = true }
  end

  local all_lines = split_lines(content)
  local total_lines = #all_lines

  local start = math.max(math.floor(offset), 1)
  local default_max = opts.max_output_lines or ctx:config("max_output_lines", DEFAULT_MAX_OUTPUT_LINES)
  local max_lines = limit == 0 and default_max or math.min(limit, default_max)
  local max_line_bytes = opts.max_line_bytes

  local lines = {}
  for i = start, math.min(start + max_lines - 1, total_lines) do
    lines[#lines + 1] = truncate_bytes(all_lines[i], max_line_bytes)
  end

  ctx:record_read(path)

  local parts = {}
  local nr_fmt = ToolView.line_nr_fmt(start + #lines - 1) .. ": %s"
  for i, line in ipairs(lines) do
    parts[#parts + 1] = string.format(nr_fmt, start + i - 1, line)
  end
  local llm_output = table.concat(parts, "\n")

  local trunc_start = start + #lines
  if trunc_start <= total_lines then
    llm_output = llm_output
      .. string.format(
        "\n\n...\n\nTruncated lines: %d-%d. Use offset=%d to read further.",
        trunc_start,
        total_lines,
        trunc_start
      )
  end

  local shown = #lines
  local annotation = shown < total_lines and string.format("%d of %d lines", shown, total_lines)
    or string.format("%d lines", shown)

  local prefix = start > 1 and table.concat(all_lines, "\n", 1, math.min(start - 1, total_lines)) or nil

  local basename = path:match("([^/]+)$")
  local parent = maki.fs.dirname(path)
  if parent and not ctx:is_instruction_file(basename) then
    ctx:load_instructions(parent)
  end

  return {
    llm_output = llm_output,
    body = build_file_view(lines, start, total_lines, path, ctx, prefix),
    annotation = annotation,
  }
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = [[
- When using the **read** tool, only read the sections you actually need.
- Use `wc -l` to check total number of lines before reading to decide a reasonable **read** tool limit.]],
})

maki.api.register_tool({
  name = "read",
  kind = "read",
  description = DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      offset = {
        type = "integer",
        description = "Line number to start from (1-indexed). Use 1 for the first line.",
        required = true,
      },
      limit = {
        type = "integer",
        description = "Max number of lines to read. Use 0 to read until end of file (capped at 2000 lines).",
        required = true,
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    buf:line({ { read_view.summary(input), "path" } })
    return buf
  end,

  restore = function(input, output, is_error, ctx)
    return read_view.render_output(input, output, ctx)
  end,

  handler = function(input, ctx)
    local raw = input.path
    if not raw then
      return { llm_output = "error: path is required", is_error = true }
    end
    local path = maki.fs.abspath(raw)
    local meta = maki.fs.metadata(path)
    if not meta then
      return { llm_output = "error: path not found: " .. path, is_error = true }
    end
    if meta.is_dir then
      return { llm_output = "error: path is a directory, use the list tool instead", is_error = true }
    end
    return read_file(path, input.offset, input.limit, ctx)
  end,
})
