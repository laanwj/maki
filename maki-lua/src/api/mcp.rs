//! `maki.mcp`: registrations the executor serves through its MCP endpoint in
//! the brain/executor split. Brain-role plugins get a loud error — the brain
//! serves nothing, so a registration there would land in a void.

use std::sync::Arc;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, Result as LuaResult, Table};

use crate::runtime::{RegisteredResource, ResourceStore};

/// Register {spec.uri} as an MCP resource this plugin serves, answered by
/// {spec.read}. The executor's endpoint lists every registered URI on
/// `resources/list` and routes a `resources/read` for one to its `read`.
///
/// `read` is called with the requested URI and follows the `(value, err)`
/// convention: a string is the resource's text content, nil plus an error
/// message fails the read.
///
/// One provider per URI; registering an already-registered URI replaces its
/// provider. `file://` URIs are refused: that scheme is the workspace file
/// corpus, served by the host itself.
///
/// @param spec table `{ uri = string, read = function }`
/// @return
/// @example
/// maki.mcp.register_resource({
///   uri = "maki://git/branch",
///   read = function() return maki.fs.git_branch(".") or "" end,
/// })
#[lua_fn]
fn register_resource(lua: &Lua, #[ctx] plugin: Arc<str>, spec: Table) -> LuaResult<()> {
    if let Some(e) = crate::role::current(lua).brain_gate("register_resource") {
        return Err(e);
    }
    let uri: String = spec.get("uri")?;
    let read: Function = spec.get("read")?;
    if !uri.contains("://") {
        return Err(mlua::Error::runtime(format!(
            "register_resource: {uri:?} is not a scheme-qualified URI"
        )));
    }
    if uri.starts_with("file://") {
        return Err(mlua::Error::runtime(
            "register_resource: file:// URIs are served from the workspace corpus",
        ));
    }
    let read = lua.create_registry_value(read)?;
    lua.app_data_mut::<ResourceStore>()
        .ok_or_else(|| mlua::Error::runtime("ResourceStore not initialized"))?
        .providers
        .insert(uri, RegisteredResource { plugin, read });
    Ok(())
}

lua_table! {
    /// MCP resource registration. What a plugin registers here, the executor
    /// serves to the brain over the split-mode MCP endpoint.
    ///
    /// ```lua
    /// maki.mcp.register_resource({
    ///   uri = "maki://git/branch",
    ///   read = function() return maki.fs.git_branch(".") or "" end,
    /// })
    /// ```
    "maki.mcp" => pub(crate) fn create_mcp_table(plugin: Arc<str>), DOCS [
        register_resource(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::HostRole;

    fn lua_with_mcp(role: Option<HostRole>) -> Lua {
        let lua = Lua::new();
        if let Some(role) = role {
            lua.set_app_data(role);
        }
        lua.set_app_data(ResourceStore::default());
        let mcp = create_mcp_table(&lua, Arc::from("test")).unwrap();
        lua.globals().set("mcp", mcp).unwrap();
        lua
    }

    fn register(lua: &Lua, uri: &str) -> mlua::Result<()> {
        lua.load(format!(
            r#"mcp.register_resource({{ uri = "{uri}", read = function() return "x" end }})"#
        ))
        .exec()
    }

    #[test]
    fn registers_and_replaces_by_uri() {
        let lua = lua_with_mcp(None);
        register(&lua, "maki://a").unwrap();
        register(&lua, "maki://b").unwrap();
        register(&lua, "maki://a").unwrap();
        let store = lua.app_data_ref::<ResourceStore>().unwrap();
        assert_eq!(store.providers.len(), 2);
    }

    #[test]
    fn refuses_file_scheme_and_bare_paths() {
        let lua = lua_with_mcp(None);
        assert!(register(&lua, "file:///etc/passwd").is_err());
        assert!(register(&lua, "no-scheme").is_err());
        assert!(
            lua.app_data_ref::<ResourceStore>()
                .unwrap()
                .providers
                .is_empty()
        );
    }

    #[test]
    fn brain_role_is_a_loud_error() {
        let lua = lua_with_mcp(Some(HostRole::Brain));
        let err = register(&lua, "maki://a").unwrap_err().to_string();
        assert!(err.contains("not available in brain-role plugins"), "{err}");
    }
}
