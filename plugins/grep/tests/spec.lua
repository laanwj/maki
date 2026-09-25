local grep_view = require("grep_view")

local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

case("parse_llm_output_reads_groups_matches_and_context", function()
  local entries = grep_view.parse_llm_output(table.concat({
    "src/main.rs:",
    "  3: fn main() {",
    "  4  helper();",
    "  --",
    "  9: helper();",
    "",
    "src/lib.rs:",
    "  1: // lib",
  }, "\n"))
  eq(#entries, 2)
  eq(entries[1].path, "src/main.rs")
  eq(#entries[1].groups, 2, "the -- line splits groups")
  eq(entries[1].groups[1].lines[1].line_nr, 3)
  eq(entries[1].groups[1].lines[1].is_match, true)
  eq(entries[1].groups[1].lines[2].is_match, false, "space separator marks context")
  eq(entries[1].groups[2].lines[1].line_nr, 9)
  eq(entries[2].path, "src/lib.rs")
end)

case("parse_llm_output_plain_text_yields_nothing", function()
  eq(#grep_view.parse_llm_output("No files found"), 0)
end)

case("view_summary_mirrors_the_header", function()
  eq(grep_view.summary({ pattern = "foo" }), "foo")
  eq(grep_view.summary({ pattern = 'foo"', include = "*.rs", path = "/tmp" }), "foo [*.rs] /tmp")
end)

th.report()
