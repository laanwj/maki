local edit_view = require("edit_view")
local shorten_path = require("maki.shorten_path")
local fuzzy_replace = require("maki.fuzzy_replace")
local replace_lines = require("edit_helpers").replace_lines
local insert_after = require("edit_helpers").insert_after
local preserve_line_endings = require("edit_helpers").preserve_line_endings

local SNIPPET_MAX_CHARS = 32

local diff_restore = edit_view.diff_restore

local EDIT_LINES_DESCRIPTION =
  [[Edit lines by number. Replaces lines from `start` to `end` (inclusive) with `new_string`. Use empty `new_string` to delete a range. Do not use with the batch tool.]]

local INSERT_LINES_DESCRIPTION =
  [[Insert `new_string` after line `line`, or at the top with 0. Only include new lines, never lines already in the file. Do not use with the batch tool.]]

local EDIT_DESCRIPTION = [[Replace an exact string match in a file.

- The old_string must appear exactly once unless replace_all is true.
- Tolerates whitespace and indentation drift in old_string; a match is always one contiguous block, never a partial prefix.
- Read the file first to get exact content.
- When copying text from read output, do NOT include the line number prefix (e.g. `42: `) - only the content after it.
- Prefer this over write for targeted changes - it uses far fewer tokens.
- Use replace_all for renaming across a file.
]]

local MULTIEDIT_DESCRIPTION = [[Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

- Read the file first to get exact content.
- old_string must match the file contents exactly, including all whitespace and indentation.
- Each edit must match exactly once unless replace_all is true. Use replace_all for renaming across a file.
- Edits are applied in sequence - each operates on the result of the previous.
- If any edit fails, none are written.
- Ensure earlier edits don't affect text that later edits need to find.
]]

local function edit_header(input)
  local buf = maki.ui.buf()
  buf:line({ { edit_view.summary(input), "path" } })
  return buf
end

local function apply_edit(path, transform)
  path = maki.fs.abspath(path)

  local before, read_err = maki.fs.read(path)
  if read_err then
    return nil, "read error: " .. tostring(read_err)
  end

  local after, transform_err = preserve_line_endings(before, transform)
  if transform_err then
    return nil, transform_err
  end

  local _, write_err = maki.fs.write(path, after)
  if write_err then
    return nil, "write error: " .. tostring(write_err)
  end

  return {
    path = path,
    before = before,
    after = after,
  }
end

local function diff_result(edit_result, summary)
  return {
    llm_output = summary,
    diff_path = edit_result.path,
    diff_before = edit_result.before,
    diff_after = edit_result.after,
    written_path = edit_result.path,
  }
end

local opts = maki.api.register_options({
  multiedit = { default = true, desc = "Provide the `multiedit` tool." },
  edit_lines = { default = true, desc = "Provide the `edit_lines` tool." },
  insert_lines = { default = false, desc = "Provide the opt-in `insert_lines` tool." },
})

local function register_tool_if(enabled, tool)
  if enabled then
    maki.api.register_tool(tool)
  end
end

maki.api.register_tool({
  name = "edit",
  kind = "edit",
  mutable_path = "path",
  permission = "fs_write",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = EDIT_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
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

  header = edit_header,
  restore = diff_restore(edit_view.blocks_edit),

  handler = function(input)
    local result, err = apply_edit(input.path, function(content)
      return fuzzy_replace.replace(content, input.old_string, input.new_string, input.replace_all or false)
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end

    return diff_result(result, "edited " .. shorten_path(result.path))
  end,
})

register_tool_if(opts.multiedit, {
  name = "multiedit",
  kind = "edit",
  mutable_path = "path",
  permission = "fs_write",
  permission_scopes = "path",
  start_annotation = "edits",
  audiences = { "main", "general_sub", "interpreter" },
  description = MULTIEDIT_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      edits = {
        type = "array",
        description = "Array of edit operations to apply sequentially",
        required = true,
        items = {
          type = "object",
          properties = {
            old_string = {
              type = "string",
              description = "Exact string to find",
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
      },
    },
  },

  header = edit_header,
  restore = diff_restore(edit_view.blocks_multiedit),

  handler = function(input)
    local edits = input.edits
    if #edits == 0 then
      return { llm_output = "provide at least one edit", is_error = true }
    end

    local result, err = apply_edit(input.path, function(content)
      for i, edit in ipairs(edits) do
        local replaced, replace_err =
          fuzzy_replace.replace(content, edit.old_string, edit.new_string, edit.replace_all or false)
        if replace_err then
          local snippet = edit.old_string:match("[^\n]*")
          local cut = utf8.offset(snippet, SNIPPET_MAX_CHARS + 1)
          if cut then
            snippet = snippet:sub(1, cut - 1) .. "…"
          end
          return nil, string.format("edits[%d] (old_string %q): %s", i - 1, snippet, replace_err)
        end
        content = replaced
      end
      return content
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end

    local n = #edits
    local s = n == 1 and "" or "s"
    return diff_result(result, string.format("applied %d edit%s to %s", n, s, shorten_path(result.path)))
  end,
})

register_tool_if(opts.edit_lines, {
  name = "edit_lines",
  kind = "edit",
  mutable_path = "path",
  permission = "fs_write",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = EDIT_LINES_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      start = {
        type = "integer",
        description = "First line (1-indexed)",
        required = true,
      },
      ["end"] = {
        type = "integer",
        description = "Last line, inclusive",
        required = true,
      },
      new_string = {
        type = "string",
        description = "Replacement text",
        required = true,
      },
    },
  },

  header = edit_header,
  restore = diff_restore(edit_view.blocks_edit_lines),

  handler = function(input)
    local result, err = apply_edit(input.path, function(content)
      return replace_lines(content, input.start, input["end"], input.new_string)
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end
    return diff_result(
      result,
      string.format("replaced lines %d-%d in %s", input.start, input["end"], shorten_path(result.path))
    )
  end,
})

register_tool_if(opts.insert_lines, {
  name = "insert_lines",
  kind = "edit",
  mutable_path = "path",
  permission = "fs_write",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
  description = INSERT_LINES_DESCRIPTION,

  schema = {
    type = "object",
    properties = {
      path = {
        type = "string",
        description = "Absolute path to the file",
        required = true,
        alias = "file_path",
      },
      line = {
        type = "integer",
        description = "Line number to insert after (1-indexed). Use 0 to insert at the top.",
        required = true,
      },
      new_string = {
        type = "string",
        description = "Text to insert",
        required = true,
      },
    },
  },

  header = edit_header,
  restore = diff_restore(edit_view.blocks_insert_lines),

  handler = function(input)
    local result, err = apply_edit(input.path, function(content)
      return insert_after(content, input.line, input.new_string)
    end)
    if not result then
      return { llm_output = err, is_error = true }
    end
    return diff_result(result, string.format("inserted after line %d in %s", input.line, shorten_path(result.path)))
  end,
})
