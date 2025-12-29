// OpenAI 数据模型

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<OpenAIMessage>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(rename = "max_tokens")]
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(rename = "top_p")]
    pub top_p: Option<f32>,
    pub stop: Option<Value>,
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(rename = "tool_choice")]
    pub tool_choice: Option<Value>,
    #[serde(rename = "parallel_tool_calls")]
    pub parallel_tool_calls: Option<bool>,
    // Codex proprietary fields
    pub instructions: Option<String>,
    pub input: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFormat {
    pub r#type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum OpenAIContent {
    String(String),
    Array(Vec<OpenAIContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum OpenAIContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
    },
    #[serde(rename = "image_url")]
    ImageUrl {
        image_url: OpenAIImageUrl,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenAIImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_content")]
    pub content: Option<OpenAIContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// 自定义反序列化：支持 content 为 null、string 或 array
fn deserialize_content<'de, D>(deserializer: D) -> Result<Option<OpenAIContent>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(None),  // 关键：null 转为 None
        Some(serde_json::Value::String(s)) => Ok(Some(OpenAIContent::String(s))),
        Some(serde_json::Value::Array(arr)) => {
            let blocks: Vec<OpenAIContentBlock> = serde_json::from_value(serde_json::Value::Array(arr))
                .map_err(|e| D::Error::custom(format!("Invalid content array: {}", e)))?;
            Ok(Some(OpenAIContent::Array(blocks)))
        }
        Some(other) => Err(D::Error::custom(format!("Invalid content type: expected null, string or array, got {:?}", other))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: OpenAIMessage,
    pub finish_reason: Option<String>,
}
