maki.api.register_command({
  name = "/newplan",
  description = "Start a fresh plan without clearing session history",
  handler = function()
    local path, err = maki.session.new_plan()
    if err then
      maki.notify(err, "error")
      return
    end
    maki.notify(path, nil, { title = "new plan" })
  end,
})
