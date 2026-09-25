+++
title = "Brain/Executor Split"
weight = 24
[extra]
group = "Guides"
+++

# Brain/Executor Split

Split Maki into two processes. The **brain** holds your API keys, talks to the providers, and drives the TUI. The **executor** owns the workspace and runs the tools. They speak [MCP](/docs/mcp/) over a unix socket, ndjson framing, and the executor has no TCP code path at all.

The point is containment. Prompt injection can steer tool calls, and the workspace stays the blast radius for that. But the keys and the conversation live in a process the model cannot reach, so raw key material has no path out.

```
┌──────────────┐         ┌───────────────────┐
│ brain         │──MCP──▶│ executor           │
│ keys, TUI,    │ socket │ workspace, tools,  │
│ session log   │        │ stdio MCP servers  │
└──────────────┘         └───────────────────┘
      │                          │
  LLM APIs + web             own egress, no keys
```

## Turn it on locally

Start the executor in the workspace:

```bash
maki serve --socket /run/maki-split/executor.sock
```

Then point the brain at it. Statically, in `~/.config/maki/mcp.toml`:

```toml
[mcp.executor]
path = "/run/maki-split/executor.sock"
```

Or per invocation, which beats the file and is how launchers wire up per-project executors:

```bash
maki --executor-socket /run/maki-split/executor.sock
MAKI_EXECUTOR_SOCKET=/run/maki-split/executor.sock maki
```

Either declares split mode, for the TUI and for the headless entry points
(`--print`, SDK mode) alike. The name `executor` is reserved in mcp.toml: any
other kind of server under it is an error.

## What moves where

Tools that touch the working environment run on the executor: `bash`, `read`, `write`, `edit`, `glob`, `grep`, `list`, `index`, `code_execution`, `skill`, `view_image`. Conversation tools stay on the brain: `task`, `question`, `todo_write`, `memory`, `batch`, `webfetch`, `websearch`. Slash commands and keymaps are brain-side always.

Two things change shape in split mode:

- The `!` shell and plugin-driven `open_editor` on workspace paths are disabled on the brain. Run your own commands on the executor host, for example `podman exec -it maki-executor bash`.
- stdio MCP servers move to the executor. The brain delegates them in the config push and tells you at startup. Their tools show up as `executor.<server>.<tool>`.

Sessions key on the executor's workspace path, which the brain learns from the executor's `maki/workspace` probe, so resume works even when the brain has no path to the workspace itself.

The probe doubles as a liveness check: if the executor does not answer at startup, the brain exits with an error instead of booting without workspace tools.

## Plugin roles

In split mode a plugin places itself by directory under the config dir:

```
autoload/executor/  tool plugins, shipped to the executor in the initialize handshake
autoload/brain/     UI plugins: commands, keymaps, tool views
```

Capabilities come from the file's leading comment block:

```lua
---@permissions run, fs_read
```

A plugin is one role. If it needs both, write two plugins. The executor needs no config mount at all, since everything it runs arrives in the handshake. The same permission keys work in `plugin.toml` for packaged plugins. Project plugins from `.maki/` only ever load on the executor, so repo-supplied code never runs next to your keys.

In single-process mode both autoload dirs load. A file that should register its tools only there can gate on `maki.fn.has("split") == 0`.

Brain-side user plugins that call `register_tool` fail loudly. That is the boundary doing its job: a registered tool's handler runs when the model calls it, and model-driven code must not execute next to keys.

## Tool views

A tool running on the executor cannot paint its own call display, because the
executor has no UI. The brain paints one instead. `maki.api.register_tool_view`
registers a brain-side view per tool name, with lifecycle callbacks that run in
the brain's Lua host and have the full `maki.ui` surface — buffers, views,
highlighting, click handlers:

```lua
maki.api.register_tool_view({
  tool = "python",
  start = function(input, ctx)
    -- paint the preview; return a per-call state value for the other two
    return { code = input.code }
  end,
  progress = function(state, payload)  -- one call per ctx:progress payload
  end,
  done = function(state, input, output, is_error)  -- paint the final body
  end,
})
```

The name is the qualified MCP name (`server.tool`), and the executor's own
tools keep their bare names. One view per tool; a later registration replaces
an earlier one, and a tool with no registered view gets generic MCP rendering.
A view also drives a single-process tool that has no `header`/`start` of its
own, so one view file serves both modes.

Executor-side, a tool emits its feed with `ctx:progress(value)`. The payload
is arbitrary JSON, and its shape is the tool's own contract with its view.

## Reference deployment## Reference deployment

`contrib/split/` has the reference material:

- `Containerfile.brain`: the static maki binary, CA roots, and micro as `$EDITOR`. No shell, no coreutils.
- `Containerfile.executor`: maki plus a base toolchain. Extend it with what your agent may run.
- `maki-executor.container`: a quadlet running the executor with a read-only rootfs, all capabilities dropped, and the workspace as its only writable mount. It gets its own unprivileged egress (dependency installs are everyday agent work, and it holds no credentials); set `Network=none` for workspaces where any egress is the concern. The socket directory is per workspace, not shared: a shared dir would let one executor drive another workspace's executor over the mount, which `Network=none` would not prevent.
- `maki-brain.sh`: runs the brain interactively, mounting the socket dir, your maki config, and your maki state dir, with keys from a host-managed env file.

The executor never initiates a connection. The brain dials. Keys reach the brain as environment variables from the env file, which maki captures and strips at startup, so no child of the brain inherits them. Their remaining exposure is same-user host tooling such as `podman inspect`, the same class as maki's own `0600` auth files.

Egress policy is yours to set. The reference gives the executor its own unprivileged egress, and `Network=none` takes it away. The brain needs the provider API hosts, plus any remote MCP endpoints, plus the web for `webfetch` and `websearch`.

## Limitations

- `@` file completion reads the brain's own file index for now, not the executor's corpus. The corpus is served over `resources/list`, and the completion popup does not read it yet.
- Prompt hints that executor plugins register do not reach the brain's system prompt yet.
- `$EDITOR` opens only the brain's own files in split mode. Plan files work, since they live in the brain's state dir.
