use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tracing::info;

use std::collections::HashMap;

use super::error::McpError;
use super::protocol::{
    CallToolResult, GetPromptResult, PromptInfo, PromptsListResult, ResourceContent, ResourceInfo,
    ResourcesListResult, ResourcesReadResult, ToolInfo, ToolsListResult, initialize_params,
};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait McpTransport: Send + Sync {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>>;
    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>>;
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
    fn server_name(&self) -> &Arc<str>;
    fn transport_kind(&self) -> &'static str;
    fn child_pids(&self) -> Vec<u32> {
        Vec::new()
    }
    /// Line-based transports route server notifications (progress, log
    /// messages) to watchers; anything else gets nothing.
    fn notification_hub(&self) -> Option<super::line::NotificationHub> {
        None
    }
}

fn invalid_response(name: &Arc<str>, e: impl std::fmt::Display) -> McpError {
    McpError::InvalidResponse {
        server: (**name).into(),
        reason: e.to_string(),
    }
}

pub struct ServerCapabilities {
    pub tools: bool,
    pub prompts: bool,
    pub resources: bool,
}

impl ServerCapabilities {
    fn parse(result: &Value) -> Self {
        Self {
            tools: result["capabilities"]["tools"].is_object(),
            prompts: result["capabilities"]["prompts"].is_object(),
            resources: result["capabilities"]["resources"].is_object(),
        }
    }
}

pub async fn initialize(transport: &dyn McpTransport) -> Result<ServerCapabilities, McpError> {
    initialize_with(transport, None).await
}

/// `initialize` with extra params merged in — the brain's executor config
/// push rides the executor's handshake this way.
pub async fn initialize_with(
    transport: &dyn McpTransport,
    extra: Option<Value>,
) -> Result<ServerCapabilities, McpError> {
    Ok(ServerCapabilities::parse(
        &initialize_full(transport, extra).await?,
    ))
}

/// The raw initialize result: extras like the executor's workspace report
/// live outside the capabilities.
pub async fn initialize_full(
    transport: &dyn McpTransport,
    extra: Option<Value>,
) -> Result<Value, McpError> {
    let mut params = initialize_params();
    if let Some(extra) = extra
        && let (Some(extra), Some(map)) = (extra.as_object(), params.as_object_mut())
    {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
    let result = transport.send_request("initialize", Some(params)).await?;
    transport
        .send_notification("notifications/initialized", None)
        .await?;
    Ok(result)
}

pub async fn list_tools(transport: &dyn McpTransport) -> Result<Vec<ToolInfo>, McpError> {
    let result = transport.send_request("tools/list", None).await?;
    let list: ToolsListResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(list.tools)
}

const METHOD_NOT_FOUND: i64 = -32601;

pub async fn list_prompts(transport: &dyn McpTransport) -> Result<Vec<PromptInfo>, McpError> {
    let result = transport.send_request("prompts/list", None).await;
    match result {
        Ok(val) => {
            let list: PromptsListResult = serde_json::from_value(val)
                .map_err(|e| invalid_response(transport.server_name(), e))?;
            Ok(list.prompts)
        }
        Err(McpError::RpcError { code, .. }) if code == METHOD_NOT_FOUND => Ok(vec![]),
        Err(e) => Err(e),
    }
}

pub async fn get_prompt(
    transport: &dyn McpTransport,
    name: &str,
    arguments: &HashMap<String, String>,
) -> Result<Vec<super::protocol::PromptMessage>, McpError> {
    let params = serde_json::json!({ "name": name, "arguments": arguments });
    let result = transport.send_request("prompts/get", Some(params)).await?;
    let parsed: GetPromptResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(parsed.messages)
}

pub async fn list_resources(transport: &dyn McpTransport) -> Result<Vec<ResourceInfo>, McpError> {
    let result = transport.send_request("resources/list", None).await;
    match result {
        Ok(val) => {
            let list: ResourcesListResult = serde_json::from_value(val)
                .map_err(|e| invalid_response(transport.server_name(), e))?;
            Ok(list.resources)
        }
        Err(McpError::RpcError { code, .. }) if code == METHOD_NOT_FOUND => Ok(vec![]),
        Err(e) => Err(e),
    }
}

pub async fn read_resource(
    transport: &dyn McpTransport,
    uri: &str,
) -> Result<Vec<ResourceContent>, McpError> {
    let params = serde_json::json!({ "uri": uri });
    let result = transport
        .send_request("resources/read", Some(params))
        .await?;
    let parsed: ResourcesReadResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;
    Ok(parsed.contents)
}

/// What a tools/call returned: the text every tool yields, plus the image
/// an image tool's answer carries (the model reads it as vision input).
#[derive(Debug)]
pub struct McpCallOutput {
    pub text: String,
    pub image: Option<maki_providers::ImageSource>,
}

pub async fn call_tool(
    transport: &dyn McpTransport,
    tool_name: &str,
    args: &Value,
) -> Result<McpCallOutput, McpError> {
    let params = serde_json::json!({
        "name": tool_name,
        "arguments": args,
    });
    call_tool_inner(transport, params).await
}

/// The chat (and subagent task) a tools/call serves; the executor keys
/// per-session tool state on it. Both `None` for sessionless callers.
#[derive(Clone, Copy, Debug, Default)]
pub struct CallRoute<'a> {
    pub session_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
}

/// tools/call with a progressToken: progress notifications for this call
/// stream out of the notification hub as ToolOutput events for `tool_id`.
pub async fn call_tool_streaming(
    transport: &dyn McpTransport,
    tool_name: &str,
    args: &Value,
    tool_id: &str,
    route: CallRoute<'_>,
    events: &crate::types::EventSender,
    payloads: Option<flume::Sender<Value>>,
) -> Result<McpCallOutput, McpError> {
    let Some(hub) = transport.notification_hub() else {
        return call_tool(transport, tool_name, args).await;
    };
    let _guard = hub.watch_progress(tool_id, Arc::from(tool_id), events.clone(), payloads);
    let mut meta = serde_json::json!({ "progressToken": tool_id });
    if let Some(sid) = route.session_id {
        meta["maki_session_id"] = Value::from(sid);
    }
    if let Some(tid) = route.task_id {
        meta["maki_task_id"] = Value::from(tid);
    }
    let params = serde_json::json!({
        "name": tool_name,
        "arguments": args,
        "_meta": meta,
    });
    call_tool_inner(transport, params).await
}

async fn call_tool_inner(
    transport: &dyn McpTransport,
    params: Value,
) -> Result<McpCallOutput, McpError> {
    let server = &**transport.server_name();
    let tool_name = params["name"].as_str().unwrap_or_default().to_owned();
    let start = Instant::now();
    let result = transport.send_request("tools/call", Some(params)).await?;
    let call_result: CallToolResult =
        serde_json::from_value(result).map_err(|e| invalid_response(transport.server_name(), e))?;

    let text = call_result.joined_text();

    if call_result.is_error {
        return Err(McpError::RpcError {
            server: (**transport.server_name()).into(),
            code: -1,
            message: text,
        });
    }

    info!(
        server,
        tool = tool_name,
        duration_ms = start.elapsed().as_millis() as u64,
        has_image = call_result.image().is_some(),
        "MCP tools/call response"
    );
    Ok(McpCallOutput {
        text,
        image: call_result.image(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    #[test_case(json!({"capabilities": {"tools": {}, "prompts": {}, "resources": {}}}), true, true, true ; "all")]
    #[test_case(json!({"capabilities": {"tools": {"listChanged": false}}}), true, false, false ; "tools_only")]
    #[test_case(json!({"capabilities": {"prompts": {}}}), false, true, false ; "prompts_only")]
    #[test_case(json!({"capabilities": {"resources": {"subscribe": true}}}), false, false, true ; "resources_only")]
    #[test_case(json!({}), false, false, false ; "no_capabilities")]
    fn parses_capabilities(result: Value, tools: bool, prompts: bool, resources: bool) {
        let caps = ServerCapabilities::parse(&result);
        assert_eq!(
            (caps.tools, caps.prompts, caps.resources),
            (tools, prompts, resources)
        );
    }
}
