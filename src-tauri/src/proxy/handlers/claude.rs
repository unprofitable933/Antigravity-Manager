// Claude 协议处理器

use axum::{
    body::Body,
    extract::{Json, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};
use tracing::{debug, error};

use crate::proxy::mappers::claude::{
    transform_claude_request_in, transform_response, create_claude_sse_stream, ClaudeRequest,
};
use crate::proxy::server::AppState;

const MAX_RETRY_ATTEMPTS: usize = 3;

/// 处理 Claude messages 请求
/// 
/// 处理 Chat 消息请求流程
pub async fn handle_messages(
    State(state): State<AppState>,
    Json(request): Json<ClaudeRequest>,
) -> Response {
    // 生成随机 Trace ID 用户追踪
    let trace_id: String = rand::Rng::sample_iter(rand::thread_rng(), &rand::distributions::Alphanumeric)
        .take(6)
        .map(char::from)
        .collect::<String>().to_lowercase();
    // 获取最新一条“有意义”的消息内容（用于日志记录和后台任务检测）
    // 策略：反向遍历，首先筛选出所有角色为 "user" 的消息，然后从中找到第一条非 "Warmup" 且非空的文本消息
    // 获取最新一条“有意义”的消息内容（用于日志记录和后台任务检测）
    // 策略：反向遍历，首先筛选出所有和用户相关的消息 (role="user")
    // 然后提取其文本内容，跳过 "Warmup" 或系统预设的 reminder
    let meaningful_msg = request.messages.iter().rev()
        .filter(|m| m.role == "user")
        .find_map(|m| {
            let content = match &m.content {
                crate::proxy::mappers::claude::models::MessageContent::String(s) => s.to_string(),
                crate::proxy::mappers::claude::models::MessageContent::Array(arr) => {
                    // 对于数组，提取所有 Text 块并拼接，忽略 ToolResult
                    arr.iter()
                        .filter_map(|block| match block {
                            crate::proxy::mappers::claude::models::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            };
            
            // 过滤规则：
            // 1. 忽略空消息
            // 2. 忽略 "Warmup" 消息
            // 3. 忽略 <system-reminder> 标签的消息
            if content.trim().is_empty() 
                || content.starts_with("Warmup") 
                || content.contains("<system-reminder>") 
            {
                None 
            } else {
                Some(content)
            }
        });

    // 如果经过过滤还是找不到（例如纯工具调用），则回退到最后一条消息的原始展示
    let latest_msg = meaningful_msg.unwrap_or_else(|| {
        request.messages.last().map(|m| {
            match &m.content {
                crate::proxy::mappers::claude::models::MessageContent::String(s) => s.clone(),
                crate::proxy::mappers::claude::models::MessageContent::Array(_) => "[Complex/Tool Message]".to_string()
            }
        }).unwrap_or_else(|| "[No Messages]".to_string())
    });
    
    
    crate::modules::logger::log_info(&format!("[{}] Received Claude request for model: {}, content_preview: {:.100}...", trace_id, request.model, latest_msg));

    // [DEBUG] 打印关键请求信息
    tracing::info!("[{}] 📥 [1/4] 接收到原始 Claude API 请求: model={}, stream={}, messages={}{}", 
        trace_id, 
        request.model, 
        request.stream, 
        request.messages.len(),
        if let Some(thinking) = &request.thinking {
            format!(", thinking={}(budget={:?})", thinking.type_, thinking.budget_tokens)
        } else {
            String::new()
        }
    );

    // 1. 获取 会话 ID (已废弃基于内容的哈希，改用 TokenManager 内部的时间窗口锁定)
    let session_id: Option<&str> = None;

    // 2. 获取 UpstreamClient
    let upstream = state.upstream.clone();
    
    // 3. 准备闭包
    let mut request_for_body = request.clone();
    let token_manager = state.token_manager;
    
    let pool_size = token_manager.len();
    let max_attempts = MAX_RETRY_ATTEMPTS.min(pool_size).max(1);

    let mut last_error = String::new();
    let mut retried_without_thinking = false;
    
    for attempt in 0..max_attempts {
        // 3. 模型路由与配置解析 (提前解析以确定请求类型)
        let mut mapped_model = crate::proxy::common::model_mapping::resolve_model_route(
            &request_for_body.model,
            &*state.custom_mapping.read().await,
            &*state.openai_mapping.read().await,
            &*state.anthropic_mapping.read().await,
        );
        // 将 Claude 工具转为 Value 数组以便探测联网
        let tools_val: Option<Vec<Value>> = request_for_body.tools.as_ref().map(|list| {
            list.iter().map(|t| serde_json::to_value(t).unwrap_or(json!({}))).collect()
        });

        let config = crate::proxy::mappers::common_utils::resolve_request_config(&request_for_body.model, &mapped_model, &tools_val);

        // 4. 获取 Token (使用准确的 request_type)
        let (access_token, project_id, email) = match token_manager.get_token(&config.request_type, false).await {
            Ok(t) => t,
            Err(e) => {
                 return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({
                        "type": "error",
                        "error": {
                            "type": "overloaded_error",
                            "message": format!("No available accounts: {}", e)
                        }
                    }))
                ).into_response();
            }
        };

        tracing::info!("Using account: {} for request (type: {})", email, config.request_type);
        
        
        // --- 核心优化：智能识别与拦截后台自动请求 ---
        // [DEBUG] 临时调试：打印原始消息以诊断提取失败
        if let Some(last_msg) = request_for_body.messages.last() {
            tracing::debug!("[{}] DEBUG - Last message role: {}, content type: {}", 
                trace_id, 
                last_msg.role,
                match &last_msg.content {
                    crate::proxy::mappers::claude::models::MessageContent::String(_) => "String",
                    crate::proxy::mappers::claude::models::MessageContent::Array(_) => "Array",
                }
            );
        }

        // [FIX] 只扫描真正的"最后一条"用户消息，且必须过滤掉系统消息
        // 关键：复用 meaningful_msg 的过滤逻辑，确保 Warmup/system-reminder 不会被当作用户请求
        let last_user_msg = request_for_body.messages.iter().rev()
            .filter(|m| m.role == "user")
            .find_map(|m| {
                let content = match &m.content {
                    crate::proxy::mappers::claude::models::MessageContent::String(s) => s.to_string(),
                    crate::proxy::mappers::claude::models::MessageContent::Array(arr) => {
                        arr.iter()
                            .filter_map(|block| match block {
                                crate::proxy::mappers::claude::models::ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                };
                
                // 过滤规则：忽略系统消息
                if content.trim().is_empty() 
                    || content.starts_with("Warmup") 
                    || content.contains("<system-reminder>") 
                {
                    None 
                } else {
                    Some(content)
                }
            })
            .unwrap_or_default();

        // [DEBUG] 打印提取结果
        tracing::debug!("[{}] DEBUG - Extracted last_user_msg length: {}, preview: {:.100}", 
            trace_id, 
            last_user_msg.len(),
            last_user_msg
        );

        // 关键词识别：标题生成、摘要提取、下一步提示建议等
        // [Optimization] 增加长度限制：真实用户提问通常不会包含这些特殊指令，且后台任务通常极短
        let preview_msg = last_user_msg.chars().take(500).collect::<String>();
        
        // [CRITICAL FIX] 强制识别系统消息为后台任务，防止它们消耗顶配额度
        let is_system_message = preview_msg.starts_with("Warmup") 
            || preview_msg.contains("<system-reminder>")
            || preview_msg.contains("Caveat: The messages below were generated by the user while running local commands");
        
        let is_background_task = is_system_message || (
            (preview_msg.contains("write a 5-10 word title") 
                || preview_msg.contains("Respond with the title")
                || preview_msg.contains("Concise summary")
                || preview_msg.contains("prompt suggestion generator"))
            && last_user_msg.len() < 800
        ); // 额外保险：后台任务通常不超过 800 字符

        // 传递映射后的模型名
        let mut request_with_mapped = request_for_body.clone();

        if is_background_task {
             mapped_model = "gemini-2.5-flash".to_string();
             tracing::info!("[{}][AUTO] 检测到后台任务 ({})，已重定向: {}", 
                trace_id,
                preview_msg,
                mapped_model
             );
             // [Optimization] **后台任务净化**: 
             // 此类任务纯粹为文本处理，绝不需要执行工具。
             // 强制清空 tools 字段，彻底根除 "Multiple tools" (400) 冲突风险。
             request_with_mapped.tools = None;
        } else {
             // [USER] 标记真实用户请求
             // [Optimization] 使用 WARN 级别高亮显示用户消息，防止被后台任务日志淹没
             tracing::warn!("[{}][USER] 检测到用户交互请求 ({:.100})，保持原模型: {}", 
                trace_id,
                preview_msg,
                mapped_model
             );
        }

        
        request_with_mapped.model = mapped_model;

        // 生成 Trace ID (简单用时间戳后缀)
        // let _trace_id = format!("req_{}", chrono::Utc::now().timestamp_subsec_millis());

        let gemini_body = match transform_claude_request_in(&request_with_mapped, &project_id) {
            Ok(b) => b,
            Err(e) => {
                 return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "type": "error",
                        "error": {
                            "type": "api_error",
                            "message": format!("Transform error: {}", e)
                        }
                    }))
                ).into_response();
            }
        };
        
        // [DEBUG] 打印发送给 Gemini 的请求摘要
        let has_thinking_config = gemini_body.get("request")
            .and_then(|r| r.get("generationConfig"))
            .and_then(|g| g.get("thinkingConfig"))
            .is_some();
        tracing::info!("[{}] 📤 [2/4] 发送给 Gemini: model={}, type={}, project={}, thinkingConfig={}", 
            trace_id,
            gemini_body.get("model").and_then(|v| v.as_str()).unwrap_or("unknown"),
            gemini_body.get("requestType").and_then(|v| v.as_str()).unwrap_or("unknown"),
            &project_id[..project_id.len().min(10)],
            has_thinking_config
        );

    // 4. 上游调用
    let is_stream = request.stream;
    let method = if is_stream { "streamGenerateContent" } else { "generateContent" };
    let query = if is_stream { Some("alt=sse") } else { None };

    let response = match upstream.call_v1_internal(
        method,
        &access_token,
        gemini_body,
        query
    ).await {
            Ok(r) => r,
            Err(e) => {
                last_error = e.clone();
                tracing::warn!("Request failed on attempt {}/{}: {}", attempt + 1, max_attempts, e);
                continue;
            }
        };
        
        let status = response.status();
        
        // 成功
        if status.is_success() {
            // 处理流式响应
            if request.stream {
                let stream = response.bytes_stream();
                let gemini_stream = Box::pin(stream);
                let claude_stream = create_claude_sse_stream(gemini_stream, trace_id);

                // 转换为 Bytes stream
                let sse_stream = claude_stream.map(|result| -> Result<Bytes, std::io::Error> {
                    match result {
                        Ok(bytes) => Ok(bytes),
                        Err(e) => Ok(Bytes::from(format!("data: {{\"error\":\"{}\"}}\n\n", e))),
                    }
                });

                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from_stream(sse_stream))
                    .unwrap();
            } else {
                // 处理非流式响应
                let bytes = match response.bytes().await {
                    Ok(b) => b,
                    Err(e) => return (StatusCode::BAD_GATEWAY, format!("Failed to read body: {}", e)).into_response(),
                };
                
                // [DEBUG] 打印从 Gemini 收到的响应摘要
                if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                        let mut parts_info = Vec::new();
                        if let Some(response_obj) = parsed.get("response") {
                            if let Some(candidates) = response_obj.get("candidates").and_then(|v| v.as_array()) {
                                if let Some(first) = candidates.get(0) {
                                    if let Some(content) = first.get("content") {
                                        if let Some(parts) = content.get("parts").and_then(|v| v.as_array()) {
                                            for part in parts {
                                                if part.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                                                    parts_info.push("thought");
                                                } else if part.get("text").is_some() {
                                                    parts_info.push("text");
                                                } else if part.get("functionCall").is_some() {
                                                    parts_info.push("functionCall");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        tracing::info!("[{}] 📥 [3/4] 从 Gemini 收到响应: {} bytes, parts=[{}]", 
                            trace_id, bytes.len(), parts_info.join(", "));
                    } else {
                        debug!("Upstream Response for Claude request: {}", text);
                    }
                }

                let gemini_resp: Value = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(e) => return (StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)).into_response(),
                };

                // 解包 response 字段（v1internal 格式）
                let raw = gemini_resp.get("response").unwrap_or(&gemini_resp);

                // 转换为 Gemini Response 结构
                let gemini_response: crate::proxy::mappers::claude::models::GeminiResponse = match serde_json::from_value(raw.clone()) {
                    Ok(r) => r,
                    Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Convert error: {}", e)).into_response(),
                };
                
                // 转换
                let claude_response = match transform_response(&gemini_response) {
                    Ok(r) => r,
                    Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("Transform error: {}", e)).into_response(),
                };

                // [DEBUG] 打印返回给客户端的响应摘要
                let mut content_types = Vec::new();
                for block in &claude_response.content {
                    match block {
                        crate::proxy::mappers::claude::models::ContentBlock::Text { .. } => content_types.push("text"),
                        crate::proxy::mappers::claude::models::ContentBlock::Thinking { .. } => content_types.push("thinking"),
                        crate::proxy::mappers::claude::models::ContentBlock::ToolUse { .. } => content_types.push("tool_use"),
                        _ => content_types.push("other"),
                    }
                }
                tracing::info!("[{}] 📤 [4/4] 返回给客户端: model={}, blocks=[{}], tokens={}+{}={}", 
                    trace_id,
                    claude_response.model,
                    content_types.join(", "),
                    claude_response.usage.input_tokens,
                    claude_response.usage.output_tokens,
                    claude_response.usage.input_tokens + claude_response.usage.output_tokens
                );
                
                // [Optimization] 记录闭环日志：消耗情况
                tracing::info!(
                    "[{}] ✅ Request finished. Model: {}, Tokens: In {}, Out {}", 
                    trace_id, 
                    request_with_mapped.model, 
                    claude_response.usage.input_tokens, 
                    claude_response.usage.output_tokens
                );

                return Json(claude_response).into_response();
            }
        }
        
        // 处理错误
        let error_text = response.text().await.unwrap_or_else(|_| format!("HTTP {}", status));
        last_error = format!("HTTP {}: {}", status, error_text);
        
        let status_code = status.as_u16();
        
        // Handle transient 429s using upstream-provided retry delay (avoid surfacing errors to clients).
        if status_code == 429 {
            if let Some(delay_ms) = crate::proxy::upstream::retry::parse_retry_delay(&error_text) {
                let actual_delay = delay_ms.saturating_add(200).min(10_000);
                tracing::warn!(
                    "Claude Upstream 429 on attempt {}/{}, waiting {}ms then retrying",
                    attempt + 1,
                    max_attempts,
                    actual_delay
                );
                sleep(Duration::from_millis(actual_delay)).await;
                continue;
            }
        }

        // Special-case 400 errors caused by invalid/foreign thinking signatures (common after /resume).
        // Retry once by stripping thinking blocks & thinking config from the request, and by disabling
        // the "-thinking" model variant if present.
        if status_code == 400
            && !retried_without_thinking
            && (error_text.contains("Invalid `signature`")
                || error_text.contains("thinking.signature: Field required")
                || error_text.contains("thinking.thinking: Field required")
                || error_text.contains("thinking.signature")
                || error_text.contains("thinking.thinking"))
        {
            retried_without_thinking = true;
            tracing::warn!("Upstream rejected thinking signature; retrying once with thinking stripped");

            // 1) Remove thinking config
            request_for_body.thinking = None;

            // 2) Remove thinking blocks from message history
            for msg in request_for_body.messages.iter_mut() {
                if let crate::proxy::mappers::claude::models::MessageContent::Array(blocks) = &mut msg.content {
                    blocks.retain(|b| !matches!(b, crate::proxy::mappers::claude::models::ContentBlock::Thinking { .. }));
                }
            }

            // 3) Prefer non-thinking Claude model variant on retry (best-effort)
            if request_for_body.model.contains("claude-") {
                let mut m = request_for_body.model.clone();
                m = m.replace("-thinking", "");
                // If it's a dated alias, fall back to a stable non-thinking id
                if m.contains("claude-sonnet-4-5-") {
                    m = "claude-sonnet-4-5".to_string();
                } else if m.contains("claude-opus-4-5-") || m.contains("claude-opus-4-") {
                    m = "claude-opus-4-5".to_string();
                }
                request_for_body.model = m;
            }

            continue;
        }

        // 只有 429 (限流), 403 (权限/地区限制) 和 401 (认证失效) 触发账号轮换
        if status_code == 429 || status_code == 403 || status_code == 401 {
            // 如果是 429 且标记为配额耗尽（明确），直接报错，避免穿透整个账号池
            if status_code == 429 && error_text.contains("QUOTA_EXHAUSTED") {
                error!("Claude Quota exhausted (429) on attempt {}/{}, stopping to protect pool.", attempt + 1, max_attempts);
                return (status, error_text).into_response();
            }

            tracing::warn!("Claude Upstream {} on attempt {}/{}, rotating account", status, attempt + 1, max_attempts);
            continue;
        }
        
        // 404 等由于模型配置或路径错误的 HTTP 异常，直接报错，不进行无效轮换
        error!("Claude Upstream non-retryable error {}: {}", status_code, error_text);
        return (status, error_text).into_response();
    }
    
    (StatusCode::TOO_MANY_REQUESTS, Json(json!({
        "type": "error",
        "error": {
            "type": "overloaded_error",
            "message": format!("All {} attempts failed. Last error: {}", max_attempts, last_error)
        }
    }))).into_response()
}

/// 列出可用模型
pub async fn handle_list_models() -> impl IntoResponse {
    Json(json!({
        "object": "list",
        "data": [
            {
                "id": "claude-sonnet-4-5",
                "object": "model",
                "created": 1706745600,
                "owned_by": "anthropic"
            },
            {
                "id": "claude-opus-4-5-thinking",
                "object": "model",
                "created": 1706745600,
                "owned_by": "anthropic"
            },
            {
                "id": "claude-3-5-sonnet-20241022",
                "object": "model",
                "created": 1706745600,
                "owned_by": "anthropic"
            }
        ]
    }))
}

/// 计算 tokens (占位符)
pub async fn handle_count_tokens(Json(_body): Json<Value>) -> impl IntoResponse {
    Json(json!({
        "input_tokens": 0,
        "output_tokens": 0
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handle_list_models() {
        let response = handle_list_models().await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
