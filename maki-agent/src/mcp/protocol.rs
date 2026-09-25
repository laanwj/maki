use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Serialize)]
pub struct JsonRpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl<'a> JsonRpcRequest<'a> {
    pub fn new(id: u64, method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

#[derive(Serialize)]
pub struct JsonRpcNotification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl<'a> JsonRpcNotification<'a> {
    pub fn new(method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Deserialize)]
pub struct JsonRpcResponse {
    pub id: Option<u64>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

pub fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": LATEST_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "maki",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// A message received by the server end: requests carry both id and method,
/// notifications only a method, anything else is junk to ignore.
#[derive(Deserialize)]
pub struct IncomingMessage {
    pub id: Option<u64>,
    pub method: Option<String>,
    pub params: Option<Value>,
}

#[derive(Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolInfo>,
}

/// MCP content blocks, the standard tagged-union shape: `{ "type": "text",
/// ... }`, `{ "type": "image", "data": ..., "mimeType": ... }`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CallToolContent {
    Text {
        text: String,
    },
    Image {
        /// base64, per the MCP schema.
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// A block type maki does not model (audio, resource links): tolerated on
    /// the wire so an unknown block never fails the call, never rendered.
    #[serde(other)]
    Unknown,
}

impl CallToolContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The image block, rebuilt as an [`ImageSource`]. An unviewable type
    /// (e.g. SVG) is dropped like a text-only answer, not an error.
    pub fn image(&self) -> Option<maki_providers::ImageSource> {
        let CallToolContent::Image { data, mime_type } = self else {
            return None;
        };
        let media_type = maki_providers::ImageMediaType::from_mime(mime_type)?;
        Some(maki_providers::ImageSource::new(
            media_type,
            std::sync::Arc::from(data.as_str()),
        ))
    }
}

#[derive(Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<CallToolContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

impl CallToolResult {
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                CallToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The first viewable image block, if the call returned one.
    pub fn image(&self) -> Option<maki_providers::ImageSource> {
        self.content.iter().find_map(|c| c.image())
    }
}

#[derive(Deserialize, Clone)]
pub struct PromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Deserialize)]
pub struct PromptInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Deserialize)]
pub struct PromptsListResult {
    pub prompts: Vec<PromptInfo>,
}

#[derive(Deserialize)]
pub struct PromptMessageContent {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    #[default]
    User,
    Assistant,
}

#[derive(Deserialize)]
pub struct PromptMessage {
    #[serde(default)]
    pub role: PromptRole,
    pub content: PromptMessageContent,
}

#[derive(Deserialize)]
pub struct GetPromptResult {
    pub messages: Vec<PromptMessage>,
}

#[derive(Serialize, Deserialize)]
pub struct ResourceInfo {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
}

#[derive(Deserialize)]
pub struct ResourcesListResult {
    pub resources: Vec<ResourceInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct ResourceContent {
    pub uri: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}

#[derive(Deserialize)]
pub struct ResourcesReadResult {
    pub contents: Vec<ResourceContent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_skips_none_params() {
        let with =
            serde_json::to_value(JsonRpcRequest::new(1, "init", Some(json!({"k": 1})))).unwrap();
        assert_eq!(with["params"]["k"], 1);

        let without = serde_json::to_value(JsonRpcRequest::new(2, "tools/list", None)).unwrap();
        assert!(without.get("params").is_none());
    }

    #[test]
    fn tool_info_honours_input_schema_rename() {
        let raw = json!({"tools": [{"name": "read_file", "description": "Read a file", "inputSchema": {"type": "object"}}]});
        let result: ToolsListResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.tools[0].name, "read_file");
        assert_eq!(result.tools[0].input_schema["type"], "object");
    }

    #[test]
    fn call_tool_result_honours_is_error_rename() {
        let raw = json!({"content": [{"type": "text", "text": "hello"}], "isError": true});
        let result: CallToolResult = serde_json::from_value(raw).unwrap();
        assert!(result.is_error);
        assert_eq!(result.joined_text(), "hello");
    }

    #[test]
    fn call_tool_result_carries_image_blocks() {
        let raw = json!({"content": [
            {"type": "text", "text": "[image: shot.png 1KB 8x8]"},
            {"type": "image", "data": "aGk=", "mimeType": "image/png"},
        ]});
        let result: CallToolResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.joined_text(), "[image: shot.png 1KB 8x8]");
        let image = result.image().expect("image block");
        assert_eq!(image.media_type, maki_providers::ImageMediaType::Png);
        assert_eq!(&*image.data, "aGk=");
    }

    /// Unviewable types (svg here) and blocks maki does not model (audio)
    /// degrade to text instead of failing the call.
    #[test]
    fn call_tool_result_tolerates_unknown_and_unviewable_blocks() {
        let raw = json!({"content": [
            {"type": "audio", "data": "AAE=", "mimeType": "audio/wav"},
            {"type": "image", "data": "aGk=", "mimeType": "image/svg+xml"},
            {"type": "text", "text": "described"},
        ]});
        let result: CallToolResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.joined_text(), "described");
        assert!(result.image().is_none(), "svg has no ImageMediaType");
    }

    #[test]
    fn prompts_list_result_deserializes() {
        let raw = json!({
            "prompts": [{
                "name": "code-review",
                "description": "Review code changes",
                "arguments": [
                    {"name": "diff", "description": "The diff to review", "required": true},
                    {"name": "style", "required": false}
                ]
            }]
        });
        let result: PromptsListResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert_eq!(result.prompts[0].name, "code-review");
        assert_eq!(
            result.prompts[0].description.as_deref(),
            Some("Review code changes")
        );
        assert_eq!(result.prompts[0].arguments.len(), 2);
        assert!(result.prompts[0].arguments[0].required);
        assert!(!result.prompts[0].arguments[1].required);
    }

    #[test]
    fn prompts_list_result_defaults() {
        let raw = json!({"prompts": [{"name": "simple"}]});
        let result: PromptsListResult = serde_json::from_value(raw).unwrap();
        assert!(result.prompts[0].description.is_none());
        assert!(result.prompts[0].arguments.is_empty());
    }

    #[test]
    fn get_prompt_result_deserializes() {
        let raw = json!({
            "messages": [
                {"role": "user", "content": {"text": "Review this code"}},
                {"role": "assistant", "content": {"text": "I'll review it"}}
            ]
        });
        let result: GetPromptResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].role, PromptRole::User);
        assert_eq!(
            result.messages[0].content.text.as_deref(),
            Some("Review this code")
        );
        assert_eq!(result.messages[1].role, PromptRole::Assistant);
    }
}
