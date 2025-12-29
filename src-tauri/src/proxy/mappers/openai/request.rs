// OpenAI → Gemini 请求转换
use super::models::*;
use serde_json::{json, Value};
use super::streaming::get_thought_signature;

pub fn transform_openai_request(request: &OpenAIRequest, project_id: &str, mapped_model: &str) -> Value {
    // 将 OpenAI 工具转为 Value 数组以便探测
    let tools_val = request.tools.as_ref().map(|list| {
        list.iter().map(|v| v.clone()).collect::<Vec<_>>()
    });

    // Resolve grounding config
    let config = crate::proxy::mappers::common_utils::resolve_request_config(&request.model, mapped_model, &tools_val);

    tracing::info!("[Debug] OpenAI Request: original='{}', mapped='{}', type='{}', has_image_config={}", 
        request.model, mapped_model, config.request_type, config.image_config.is_some());
    
    // 1. 提取所有 System Message 并注入补丁
    let mut system_instructions: Vec<String> = request.messages.iter()
        .filter(|msg| msg.role == "system")
        .filter_map(|msg| {
            msg.content.as_ref().map(|c| match c {
                OpenAIContent::String(s) => s.clone(),
                OpenAIContent::Array(blocks) => {
                    blocks.iter().filter_map(|b| {
                        if let OpenAIContentBlock::Text { text } = b {
                            Some(text.clone())
                        } else {
                            None
                        }
                    }).collect::<Vec<_>>().join("\n")
                }
            })
        })
        .collect();

    // 注入 Codex/Coding Agent 补丁
    system_instructions.push("You are a coding agent. You MUST use the provided 'shell' tool to perform ANY filesystem operations (reading, writing, creating files). Do not output JSON code blocks for tool execution; invoke the functions directly. To create a file, use the 'shell' tool with 'New-Item' or 'Set-Content' (Powershell). NEVER simulate/hallucinate actions in text without calling the tool first.".to_string());

    // Pre-scan to map tool_call_id to function name (for Codex)
    let mut tool_id_to_name = std::collections::HashMap::new();
    for msg in &request.messages {
        if let Some(tool_calls) = &msg.tool_calls {
            for call in tool_calls {
                let name = &call.function.name;
                let final_name = if name == "local_shell_call" { "shell" } else { name };
                tool_id_to_name.insert(call.id.clone(), final_name.to_string());
            }
        }
    }

    // 从全局存储获取 thoughtSignature (PR #93 支持)
    let global_thought_sig = get_thought_signature();
    if global_thought_sig.is_some() {
        tracing::info!("从全局存储获取到 thoughtSignature (长度: {})", global_thought_sig.as_ref().unwrap().len());
    }

    // 2. 构建 Gemini contents (过滤掉 system)
    let contents: Vec<Value> = request
        .messages
        .iter()
        .filter(|msg| msg.role != "system")
        .map(|msg| {
            let role = match msg.role.as_str() {
                "assistant" => "model",
                "tool" | "function" => "user", 
                _ => &msg.role,
            };

            let mut parts = Vec::new();
            
            // Handle content (multimodal or text)
            if let Some(content) = &msg.content {
                match content {
                    OpenAIContent::String(s) => {
                        if !s.is_empty() {
                            if role == "user" && mapped_model.contains("gemini-3") {
                                // 为 Gemini 3 用户消息添加提醒补丁
                                let reminder = "\n\n(SYSTEM REMINDER: You MUST use the 'shell' tool to perform this action. Do not simply state it is done.)";
                                parts.push(json!({"text": format!("{}{}", s, reminder)}));
                            } else {
                                parts.push(json!({"text": s}));
                            }
                        }
                    }
                    OpenAIContent::Array(blocks) => {
                        for block in blocks {
                            match block {
                                OpenAIContentBlock::Text { text } => {
                                    if role == "user" && mapped_model.contains("gemini-3") {
                                        let reminder = "\n\n(SYSTEM REMINDER: You MUST use the 'shell' tool to perform this action. Do not simply state it is done.)";
                                        parts.push(json!({ "text": format!("{}{}", text, reminder) }));
                                    } else {
                                        parts.push(json!({"text": text}));
                                    }
                                }
                                OpenAIContentBlock::ImageUrl { image_url } => {
                                    if image_url.url.starts_with("data:") {
                                        if let Some(pos) = image_url.url.find(",") {
                                            let mime_part = &image_url.url[5..pos];
                                            let mime_type = mime_part.split(';').next().unwrap_or("image/jpeg");
                                            let data = &image_url.url[pos + 1..];
                                            
                                            parts.push(json!({
                                                "inlineData": { "mimeType": mime_type, "data": data }
                                            }));
                                        }
                                    } else if image_url.url.starts_with("http") {
                                        parts.push(json!({
                                            "fileData": { "fileUri": &image_url.url, "mimeType": "image/jpeg" }
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Handle tool calls (assistant message)
            if let Some(tool_calls) = &msg.tool_calls {
                for (index, tc) in tool_calls.iter().enumerate() {
                    // Inject Thought before function call (PR #93)
                    if index == 0 && parts.is_empty() {
                         if mapped_model.contains("gemini-3") {
                              parts.push(json!({"text": "Thinking Process: Determining necessary tool actions."}));
                         }
                    }

                    let args = serde_json::from_str::<Value>(&tc.function.arguments).unwrap_or(json!({}));
                    let mut func_call_part = json!({
                        "functionCall": {
                            "name": if tc.function.name == "local_shell_call" { "shell" } else { &tc.function.name },
                            "args": args
                        }
                    });

                    // [修复] 为该消息内的所有工具调用注入 thoughtSignature (PR #114 优化)
                    if let Some(ref sig) = global_thought_sig {
                        func_call_part["thoughtSignature"] = json!(sig);
                    }

                    parts.push(func_call_part);
                }
            }

            // Handle tool response
            if msg.role == "tool" || msg.role == "function" {
                let name = msg.name.as_deref().unwrap_or("unknown");
                let final_name = if name == "local_shell_call" { "shell" } 
                                else if let Some(id) = &msg.tool_call_id { tool_id_to_name.get(id).map(|s| s.as_str()).unwrap_or(name) }
                                else { name };

                let content_val = match &msg.content {
                    Some(OpenAIContent::String(s)) => s.clone(),
                    Some(OpenAIContent::Array(blocks)) => blocks.iter().filter_map(|b| if let OpenAIContentBlock::Text { text } = b { Some(text.clone()) } else { None }).collect::<Vec<_>>().join("\n"),
                    None => "".to_string()
                };

                parts.push(json!({
                    "functionResponse": {
                       "name": final_name,
                       "id": msg.tool_call_id.as_deref().unwrap_or("unknown"),
                       "response": { "result": content_val }
                    }
                }));
            }

            json!({ "role": role, "parts": parts })
        })
        .collect();

    // 3. 构建请求体
    let mut gen_config = json!({
        "maxOutputTokens": request.max_tokens.unwrap_or(64000),
        "temperature": request.temperature.unwrap_or(1.0),
        "topP": request.top_p.unwrap_or(1.0), 
    });

    if let Some(stop) = &request.stop {
        if stop.is_string() { gen_config["stopSequences"] = json!([stop]); }
        else if stop.is_array() { gen_config["stopSequences"] = stop.clone(); }
    }

    if let Some(fmt) = &request.response_format {
        if fmt.r#type == "json_object" {
            gen_config["responseMimeType"] = json!("application/json");
        }
    }

    let mut inner_request = json!({
        "contents": contents,
        "generationConfig": gen_config,
        "safetySettings": [
            { "category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF" },
            { "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF" },
            { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF" },
            { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF" },
            { "category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "OFF" },
        ]
    });

    // 深度清理 [undefined] 字符串 (Cherry Studio 等客户端常见注入)
    crate::proxy::mappers::common_utils::deep_clean_undefined(&mut inner_request);

    // 4. Handle Tools (Merged Cleaning)
    if let Some(tools) = &request.tools {
        let mut function_declarations: Vec<Value> = Vec::new();
        for tool in tools.iter() {
            let mut gemini_func = if let Some(func) = tool.get("function") {
                func.clone()
            } else {
                let mut func = tool.clone();
                if let Some(obj) = func.as_object_mut() {
                    obj.remove("type");
                    obj.remove("strict");
                    obj.remove("additionalProperties");
                }
                func
            };

            if let Some(name) = gemini_func.get("name").and_then(|v| v.as_str()) {
                // 跳过内置联网工具名称，避免重复定义
                if name == "web_search" || name == "google_search" || name == "web_search_20250305" {
                    continue;
                }
                
                if name == "local_shell_call" {
                    if let Some(obj) = gemini_func.as_object_mut() {
                        obj.insert("name".to_string(), json!("shell"));
                    }
                }
            }

            // [修复] 将 Anthropic 格式的 input_schema 重命名为 Gemini 格式的 parameters
            if let Some(obj) = gemini_func.as_object_mut() {
                if let Some(input_schema) = obj.remove("input_schema") {
                    obj.insert("parameters".to_string(), input_schema);
                }
            }

            if let Some(params) = gemini_func.get_mut("parameters") {
                // 先应用全局清洗
                crate::proxy::common::json_schema::clean_json_schema(params);
                // 再应用 Gemini 专有映射 (PR #93)
                if let Some(params_obj) = params.as_object_mut() {
                    if !params_obj.contains_key("type") {
                        params_obj.insert("type".to_string(), json!("OBJECT"));
                    }
                }
                map_json_schema_to_gemini(params);
            }

            // [修复] 只保留 Gemini functionDeclarations 支持的字段
            if let Some(obj) = gemini_func.as_object_mut() {
                let allowed_keys: std::collections::HashSet<&str> =
                    ["name", "description", "parameters"].iter().cloned().collect();
                obj.retain(|k, _| allowed_keys.contains(k.as_str()));
            }

            function_declarations.push(gemini_func);
        }
        
        if !function_declarations.is_empty() {
            inner_request["tools"] = json!([{ "functionDeclarations": function_declarations }]);
        }
    }
    
    if !system_instructions.is_empty() {
        inner_request["systemInstruction"] = json!({ "parts": [{"text": system_instructions.join("\n\n")}] });
    }
    
    if config.inject_google_search {
        crate::proxy::mappers::common_utils::inject_google_search_tool(&mut inner_request);
    }

    if let Some(image_config) = config.image_config {
         if let Some(obj) = inner_request.as_object_mut() {
             obj.remove("tools");
             obj.remove("systemInstruction");
             let gen_config = obj.entry("generationConfig").or_insert_with(|| json!({}));
             if let Some(gen_obj) = gen_config.as_object_mut() {
                 gen_obj.remove("thinkingConfig");
                 gen_obj.remove("responseMimeType"); 
                 gen_obj.remove("responseModalities");
                 gen_obj.insert("imageConfig".to_string(), image_config);
             }
         }
    }

    json!({
        "project": project_id,
        "requestId": format!("openai-{}", uuid::Uuid::new_v4()),
        "request": inner_request,
        "model": config.final_model,
        "userAgent": "antigravity",
        "requestType": config.request_type
    })
}

fn map_json_schema_to_gemini(value: &mut Value) {
    if let Some(obj) = value.as_object_mut() {
        let allowed_keys = ["type", "description", "properties", "required", "items", "enum", "format", "nullable"];
        obj.retain(|k, _| allowed_keys.contains(&k.as_str()));

        let type_str = obj.get("type").and_then(|t| t.as_str()).map(|s| s.to_string());
        if let Some(s) = type_str {
            obj.insert("type".to_string(), json!(s.to_uppercase()));
        }
        
        if let Some(properties) = obj.get_mut("properties") {
            if let Some(props_obj) = properties.as_object_mut() {
                for (_, prop_val) in props_obj {
                    map_json_schema_to_gemini(prop_val);
                }
            }
        }
        
        if let Some(items) = obj.get_mut("items") {
             map_json_schema_to_gemini(items);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transform_openai_request_multimodal() {
        let req = OpenAIRequest {
            model: "gpt-4-vision".to_string(),
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: Some(OpenAIContent::Array(vec![
                    OpenAIContentBlock::Text { text: "What is in this image?".to_string() },
                    OpenAIContentBlock::ImageUrl { image_url: OpenAIImageUrl { 
                        url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==".to_string(),
                        detail: None 
                    } }
                ])),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            stream: false,
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            response_format: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            instructions: None,
            input: None,
            prompt: None,
        };

        let result = transform_openai_request(&req, "test-v", "gemini-1.5-flash");
        let parts = &result["request"]["contents"][0]["parts"];
        assert_eq!(parts.as_array().unwrap().len(), 2);
        assert_eq!(parts[0]["text"].as_str().unwrap(), "What is in this image?");
        assert_eq!(parts[1]["inlineData"]["mimeType"].as_str().unwrap(), "image/png");
    }
}
