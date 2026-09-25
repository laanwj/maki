local grep_view = require("grep_view")
local truncate = require("maki.truncate")
local shorten_path = require("maki.shorten_path")
local output_limits = require("maki.output_limits")

local NO_MATCHES = "No files found"
local MAX_PER_CALL_LIMIT = 1000

local opts = maki.api.register_options(output_limits.extend({
  search_result_limit = {
    default = 100,
    min = 10,
    desc = "Max match groups per search. A call's `limit` param overrides it.",
  },
  max_line_bytes = { default = 500, min = 80, desc = "Skip lines longer than this many bytes." },
}))

local function has_context(groups)
  for _, group in ipairs(groups) do
    if #group.lines > 1 then
      return true
    end
  end
  return false
end

local build_grep_view = grep_view.build
local parse_llm_output = grep_view.parse_llm_output

local function format_llm_output(entries)
  local parts = {}
  for i, entry in ipairs(entries) do
    if i > 1 then
      parts[#parts + 1] = ""
    end
    parts[#parts + 1] = entry.path .. ":"
    local ctx = has_context(entry.groups)
    for gi, group in ipairs(entry.groups) do
      if gi > 1 and ctx then
        parts[#parts + 1] = "  --"
      end
      for _, line in ipairs(group.lines) do
        local sep = line.is_match and ":" or " "
        parts[#parts + 1] = string.format("  %d%s %s", line.line_nr, sep, line.text)
      end
    end
  end
  return table.concat(parts, "\n")
end

local function count_matches(entries)
  local matches = 0
  for _, entry in ipairs(entries) do
    for _, group in ipairs(entry.groups) do
      for _, line in ipairs(group.lines) do
        if line.is_match then
          matches = matches + 1
        end
      end
    end
  end
  local f = #entries == 1 and "file" or "files"
  return string.format("%d matches in %d %s", matches, #entries, f)
end

maki.api.register_prompt_hint({
  slot = "tool_usage",
  content = "- Use the **grep** tool when searching for specific content across files.",
})

maki.api.register_tool({
  name = "grep",
  kind = "search",
  description = [[Search file contents using regex.

- Respects .gitignore.
- Results grouped by file, sorted by modification time.
- Prefer speculative parallel searches over sequential rounds of glob+grep.
- Do NOT wrap the pattern in quotes. Do NOT double-escape (e.g. `\[` not `\\[`).
- Multi-line matching is auto-enabled when the pattern contains `\n`, `(?s)`, or `(?m)`.
- Do not use grep for structural code queries, it's error-prone. Use the `python` tool with `tree_sitter`.]],

  schema = {
    type = "object",
    properties = {
      pattern = { type = "string", description = "Regex pattern", required = true },
      path = { type = "string", description = "Directory to search in (default: cwd)" },
      include = {
        type = "string",
        description = "File glob filter (e.g. *.c)",
        alias = "glob",
      },
      context_before = { type = "integer", description = "Context lines before match" },
      context_after = { type = "integer", description = "Context lines after match" },
      limit = { type = "integer", description = "Max match groups to return" },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    local pattern = (input.pattern or ""):gsub('"$', "")
    local spans = { { pattern, "tool" } }
    if input.include then
      spans[#spans + 1] = { " [" .. input.include .. "]", "dim" }
    end
    if input.path then
      spans[#spans + 1] = { " " .. shorten_path(input.path), "path" }
    end
    buf:line(spans)
    return buf
  end,

  restore = function(_input, output, _is_error, ctx)
    local entries = parse_llm_output(output)
    if #entries == 0 then
      return nil
    end
    return build_grep_view(entries, ctx)
  end,

  handler = function(input, ctx)
    local pattern = input.pattern
    if not pattern then
      return { llm_output = "error: pattern is required", is_error = true }
    end
    pattern = pattern:gsub('"$', "")

    local max_lines, max_bytes = output_limits.resolve(opts, ctx)

    local limit = math.min(input.limit or opts.search_result_limit, MAX_PER_CALL_LIMIT)

    local max_line_bytes = opts.max_line_bytes

    local entries, err = maki.fs.grep(pattern, {
      path = input.path,
      include = input.include,
      context_before = input.context_before or 0,
      context_after = input.context_after or 0,
      limit = limit,
      max_line_bytes = max_line_bytes,
    })

    if not entries then
      return { llm_output = "error: " .. tostring(err), is_error = true }
    end

    if #entries == 0 then
      return { llm_output = NO_MATCHES }
    end

    for _, entry in ipairs(entries) do
      ctx:record_read(entry.path)
    end

    local llm_output = format_llm_output(entries)
    llm_output = truncate(llm_output, max_lines, max_bytes)

    return {
      llm_output = llm_output,
      body = build_grep_view(entries, ctx),
      annotation = count_matches(entries),
    }
  end,
})
