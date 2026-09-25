local write_view = require("write_view")
local shorten_path = require("maki.shorten_path")
local ToolView = require("maki.tool_view")

local DESCRIPTION = [[Write content to a file, replacing existing content.

- Creates parent directories if needed.
- Always read the file first before writing.]]

local write_view_opts = write_view.view_opts
local build_view = write_view.build_view

maki.api.register_tool({
  name = "write",
  kind = "edit",
  mutable_path = "path",
  permission = "fs_write",
  permission_scopes = "path",
  audiences = { "main", "general_sub", "interpreter" },
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
      content = {
        type = "string",
        description = "The complete file content to write",
        required = true,
      },
      append = {
        type = "boolean",
        description = "Add content to the end of the file instead of replacing it",
      },
    },
  },

  header = function(input)
    local buf = maki.ui.buf()
    buf:line({ { write_view.summary(input), "path" } })
    return buf
  end,

  restore = function(input, output, _is_error, ctx)
    local content = input.content or ""
    if content == "" then
      return ToolView.restore(output, write_view_opts(ctx))
    end
    return build_view(content, input.path or "", ctx)
  end,

  handler = function(input, ctx)
    local raw = input.path
    if not raw then
      return { llm_output = "error: path is required", is_error = true }
    end
    local content = input.content
    if not content then
      return { llm_output = "error: content is required", is_error = true }
    end

    local path = maki.fs.abspath(raw)

    local parent = maki.fs.dirname(path)
    if parent then
      maki.fs.mkdir(parent, { parents = true })
    end

    local write = input.append and maki.fs.append or maki.fs.write
    local _, write_err = write(path, content)
    if write_err then
      return { llm_output = "write error: " .. tostring(write_err), is_error = true }
    end

    local byte_count = #content
    local rel = shorten_path(path)
    local llm_output = string.format("wrote %d bytes to %s", byte_count, rel)
    local annotation = string.format("%d bytes", byte_count)

    return {
      llm_output = llm_output,
      body = build_view(content, path, ctx),
      annotation = annotation,
      written_path = path,
    }
  end,
})
