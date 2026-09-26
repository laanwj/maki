-- Serves the workspace's git branch as an MCP resource. A split-mode brain
-- has no .git of its own; its status bar fetches this instead. The URI is
-- maki_agent::git::BRANCH_RESOURCE_URI on the Rust side.
maki.mcp.register_resource({
  uri = "maki://git/branch",
  read = function()
    -- The executor's cwd is the workspace. "" reads as a branchless label
    -- brain-side, so "not a repository" is content, not an error.
    return maki.fs.git_branch(".") or ""
  end,
})
