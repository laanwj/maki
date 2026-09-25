local helpers = require("read_helpers")

local truncate_bytes = helpers.truncate_bytes
local split_lines = helpers.split_lines

local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

case("truncate_ascii", function()
  eq(truncate_bytes("", 10), "")
  eq(truncate_bytes("hello", 10), "hello")
  eq(truncate_bytes("hello", 5), "hello")
  eq(truncate_bytes("hello world", 5), "hello...")
  eq(truncate_bytes("ab", 1), "a...")
end)

case("truncate_utf8_boundary_safety", function()
  -- 2-byte: é = \xC3\xA9
  eq(truncate_bytes("caf\xC3\xA9", 10), "caf\xC3\xA9")
  eq(truncate_bytes("caf\xC3\xA9!", 5), "caf...")
  eq(truncate_bytes("caf\xC3\xA9", 4), "caf...")

  -- 3-byte: € = \xE2\x82\xAC — cut at each byte within the sequence
  eq(truncate_bytes("ab\xE2\x82\xACd", 5), "ab...")
  eq(truncate_bytes("ab\xE2\x82\xAC", 4), "ab...")
  eq(truncate_bytes("ab\xE2\x82\xAC", 3), "ab...")

  -- 4-byte: 🎉 = \xF0\x9F\x8E\x89 — cutting anywhere inside removes entire char
  local emoji = "\xF0\x9F\x8E\x89"
  eq(truncate_bytes(emoji, 4), emoji)
  eq(truncate_bytes(emoji, 3), "...")
  eq(truncate_bytes(emoji, 1), "...")

  -- all multibyte: cutting within sequences
  local s = "\xC3\xA9\xC3\xA9\xC3\xA9"
  eq(truncate_bytes(s, 4), "\xC3\xA9...")
  eq(truncate_bytes(s, 2), "...")
end)

case("split_lines", function()
  local vectors = {
    { "", 0, {} },
    { "hello", 1, { "hello" } },
    { "a\nb", 2, { "a", "b" } },
    { "a\nb\n", 2, { "a", "b" } },
    { "\n\n\n", 3, { "", "", "" } },
    { "a\r\nb\r\n", 2, { "a", "b" } },
  }
  for _, v in ipairs(vectors) do
    local lines = split_lines(v[1])
    eq(#lines, v[2], "count for " .. ("%q"):format(v[1]))
    for i, expected in ipairs(v[3]) do
      eq(lines[i], expected, "line " .. i .. " for " .. ("%q"):format(v[1]))
    end
  end
end)

-- read_view: the output parsing the brain-side view rebuilds bodies from

case("view_parse_output_reads_numbered_lines_and_truncation", function()
  local read_view = require("read_view")
  local lines, start_line, total =
    read_view.parse_output(" 12: alpha\n13: beta\n\n...\n\nTruncated lines: 12-30. Use offset=12 to read further.")
  eq(#lines, 2)
  eq(lines[1], "alpha")
  eq(lines[2], "beta")
  eq(start_line, 12)
  eq(total, 30)
end)

case("view_parse_output_unnumbered_output_yields_nothing", function()
  local read_view = require("read_view")
  local lines, start_line, total = read_view.parse_output("just text")
  eq(#lines, 0)
  eq(start_line, nil)
  eq(total, nil)
end)

case("view_summary_mirrors_the_header_range", function()
  local read_view = require("read_view")
  eq(read_view.summary({ path = "x.rs", offset = 1, limit = 10 }), "x.rs:1-10")
  eq(read_view.summary({ path = "x.rs", offset = 5, limit = 0 }), "x.rs:5")
end)

th.report()
