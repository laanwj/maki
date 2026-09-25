local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local function repo_with_head(head)
  local dir = th.mktmpdir("git_spec")
  assert(maki.fs.mkdir(dir .. "/.git"))
  assert(maki.fs.write(dir .. "/.git/HEAD", head))
  return dir
end

case("git_branch reads a symref", function()
  local dir = repo_with_head("ref: refs/heads/feature/foo\n")
  eq(maki.fs.git_branch(dir), "feature/foo")
  th.rmtree(dir)
end)

case("git_branch shortens a detached HEAD", function()
  local dir = repo_with_head("abc1234deadbeef\n")
  eq(maki.fs.git_branch(dir), "abc1234")
  th.rmtree(dir)
end)

case("git_branch finds the repository from a subdirectory", function()
  local dir = repo_with_head("ref: refs/heads/main\n")
  assert(maki.fs.mkdir(dir .. "/sub"))
  eq(maki.fs.git_branch(dir .. "/sub"), "main")
  th.rmtree(dir)
end)

case("git_branch is nil outside a repository", function()
  local dir = th.mktmpdir("git_spec")
  eq(maki.fs.git_branch(dir), nil)
  th.rmtree(dir)
end)

th.report()
