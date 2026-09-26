//! Brain/executor roles for plugins and plugin hosts (the brain/executor split).

/// Which side of the brain/executor split a plugin belongs to. Single-process
/// maki makes no distinction: every surface exists and declarations are ignored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PluginRole {
    Brain,
    Executor,
}

impl PluginRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Brain => "brain",
            Self::Executor => "executor",
        }
    }
}

/// The role a plugin host runs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostRole {
    SingleProcess,
    Brain,
    Executor,
}

impl HostRole {
    pub fn is_executor(self) -> bool {
        matches!(self, Self::Executor)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleProcess => "single-process",
            Self::Brain => "brain",
            Self::Executor => "executor",
        }
    }

    /// Executor-role plugins get no TUI surface; the error is loud on purpose
    /// so a misplaced plugin fails at load instead of silently no-opping.
    pub(crate) fn executor_gate(self, what: &str) -> Option<mlua::Error> {
        self.is_executor().then(|| executor_gate_error(what))
    }

    /// The brain serves no MCP endpoint; registering executor-served state
    /// there would land in a void, so it fails loudly instead.
    pub(crate) fn brain_gate(self, what: &str) -> Option<mlua::Error> {
        matches!(self, Self::Brain)
            .then(|| mlua::Error::runtime(format!("{what}: not available in brain-role plugins")))
    }
}

/// The host role for this Lua state, set once at runtime creation.
/// Single-process maki never sets it and defaults to SingleProcess.
pub(crate) fn current(lua: &mlua::Lua) -> HostRole {
    lua.app_data_ref::<HostRole>()
        .map(|r| *r)
        .unwrap_or(HostRole::SingleProcess)
}

pub(crate) fn executor_gate_error(what: &str) -> mlua::Error {
    mlua::Error::runtime(format!("{what}: not available in executor-role plugins"))
}
