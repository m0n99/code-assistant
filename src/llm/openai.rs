use crate::llm::{
    types::*, ApiError, ApiErrorContext, LLMProvider, RateLimitHandler, StreamingCallback,
};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

#[derive(Debug, Serialize, Clone)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIChatMessage>,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize, Clone)]
struct StreamOptions {
    include_usage: bool,
}

impl OpenAIRequest {
    fn into_streaming(mut self) -> Self {
        self.stream = Some(true);
        self.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        self
    }

    fn into_non_streaming(mut self) -> Self {
        self.stream = None;
        self.stream_options = None;
        self
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAIChatMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCall>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct OpenAIFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
    usage: OpenAIUsage,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIChatMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamResponse {
    choices: Vec<OpenAIStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIStreamChoice {
    delta: OpenAIDelta,
    #[serde(rename = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIDelta {
    #[serde(default)]
    content: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCallDelta>>,
}

#[derive(Debug, Deserialize, Clone)]
struct OpenAIToolCallDelta {
    #[allow(dead_code)]
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    #[serde(default)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<OpenAIFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[allow(dead_code)]
    total_tokens: u32,
}

#[derive(Debug, Deserialize, Clone)]
struct OpenAIFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIErrorResponse {
    error: OpenAIError,
}

#[derive(Debug, Deserialize)]
struct OpenAIError {
    message: String,
    #[serde(rename = "type")]
    code: Option<String>,
}

/// Rate limit information extracted from response headers
#[derive(Debug)]
struct OpenAIRateLimitInfo {
    requests_limit: Option<u32>,
    requests_remaining: Option<u32>,
    requests_reset: Option<Duration>,
    tokens_limit: Option<u32>,
    tokens_remaining: Option<u32>,
    tokens_reset: Option<Duration>,
}

impl RateLimitHandler for OpenAIRateLimitInfo {
    fn from_response(response: &Response) -> Self {
        let headers = response.headers();

        fn parse_header<T: std::str::FromStr>(
            headers: &reqwest::header::HeaderMap,
            name: &str,
        ) -> Option<T> {
            headers
                .get(name)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse().ok())
        }

        fn parse_duration(headers: &reqwest::header::HeaderMap, name: &str) -> Option<Duration> {
            headers
                .get(name)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| {
                    // Parse OpenAI's duration format (e.g., "1s", "6m0s")
                    let mut seconds = 0u64;
                    let mut current_num = String::new();

                    for c in s.chars() {
                        match c {
                            '0'..='9' => current_num.push(c),
                            'm' => {
                                if let Ok(mins) = current_num.parse::<u64>() {
                                    seconds += mins * 60;
                                }
                                current_num.clear();
                            }
                            's' => {
                                if let Ok(secs) = current_num.parse::<u64>() {
                                    seconds += secs;
                                }
                                current_num.clear();
                            }
                            _ => current_num.clear(),
                        }
                    }
                    Some(Duration::from_secs(seconds))
                })
        }

        Self {
            requests_limit: parse_header(headers, "x-ratelimit-limit-requests"),
            requests_remaining: parse_header(headers, "x-ratelimit-remaining-requests"),
            requests_reset: parse_duration(headers, "x-ratelimit-reset-requests"),
            tokens_limit: parse_header(headers, "x-ratelimit-limit-tokens"),
            tokens_remaining: parse_header(headers, "x-ratelimit-remaining-tokens"),
            tokens_reset: parse_duration(headers, "x-ratelimit-reset-tokens"),
        }
    }

    fn get_retry_delay(&self) -> Duration {
        // Take the longer of the two reset times if both are present
        let mut delay = Duration::from_secs(2); // Default fallback

        if let Some(requests_reset) = self.requests_reset {
            delay = delay.max(requests_reset);
        }

        if let Some(tokens_reset) = self.tokens_reset {
            delay = delay.max(tokens_reset);
        }

        // Add a small buffer
        delay + Duration::from_secs(1)
    }

    fn log_status(&self) {
        debug!(
            "OpenAI Rate limits - Requests: {}/{} (reset in: {}s), Tokens: {}/{} (reset in: {}s)",
            self.requests_remaining
                .map_or("?".to_string(), |r| r.to_string()),
            self.requests_limit
                .map_or("?".to_string(), |l| l.to_string()),
            self.requests_reset.map_or(0, |d| d.as_secs()),
            self.tokens_remaining
                .map_or("?".to_string(), |r| r.to_string()),
            self.tokens_limit.map_or("?".to_string(), |l| l.to_string()),
            self.tokens_reset.map_or(0, |d| d.as_secs()),
        );
    }
}

pub struct OpenAIClient {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenAIClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            model,
        }
    }

    #[cfg(test)]
    pub fn new_with_base_url(api_key: String, model: String, base_url: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url,
            model,
        }
    }

    fn get_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn convert_message(message: &Message) -> OpenAIChatMessage {
        OpenAIChatMessage {
            role: match message.role {
                MessageRole::User => "user".to_string(),
                MessageRole::Assistant => "assistant".to_string(),
            },
            content: match &message.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Structured(_) => {
                    // For now, we'll just convert structured content to a simple text message
                    // This could be enhanced to handle OpenAI's specific formats
                    "[Structured content not supported]".to_string()
                }
            },
            tool_calls: None,
        }
    }

    async fn send_with_retry(
        &self,
        request: &OpenAIRequest,
        streaming_callback: Option<&StreamingCallback>,
        max_retries: u32,
    ) -> Result<LLMResponse> {
        let mut attempts = 0;

        loop {
            match if let Some(callback) = streaming_callback {
                self.try_send_request_streaming(request, callback).await
            } else {
                self.try_send_request(request).await
            } {
                Ok((response, rate_limits)) => {
                    rate_limits.log_status();
                    return Ok(response);
                }
                Err(e) => {
                    let rate_limits = e
                        .downcast_ref::<ApiErrorContext<OpenAIRateLimitInfo>>()
                        .and_then(|ctx| ctx.rate_limits.as_ref());

                    match e.downcast_ref::<ApiError>() {
                        Some(ApiError::RateLimit(_)) => {
                            if let Some(rate_limits) = rate_limits {
                                if attempts < max_retries {
                                    attempts += 1;
                                    let delay = rate_limits.get_retry_delay();
                                    warn!(
                                        "OpenAI rate limit hit (attempt {}/{}), waiting {} seconds before retry",
                                        attempts,
                                        max_retries,
                                        delay.as_secs()
                                    );
                                    sleep(delay).await;
                                    continue;
                                }
                            }
                        }
                        Some(ApiError::ServiceError(_)) | Some(ApiError::NetworkError(_)) => {
                            if attempts < max_retries {
                                attempts += 1;
                                let delay = Duration::from_secs(2u64.pow(attempts - 1));
                                warn!(
                                    "Error: {} (attempt {}/{}), retrying in {} seconds",
                                    e,
                                    attempts,
                                    max_retries,
                                    delay.as_secs()
                                );
                                sleep(delay).await;
                                continue;
                            }
                        }
                        _ => {} // Don't retry other types of errors
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn check_response_error(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            debug!("Response status is success");
            return Ok(response);
        }

        let rate_limits = OpenAIRateLimitInfo::from_response(&response);
        let response_text = response
            .text()
            .await
            .map_err(|e| ApiError::NetworkError(e.to_string()))?;

        let error = if let Ok(error_response) =
            serde_json::from_str::<OpenAIErrorResponse>(&response_text)
        {
            match (status, error_response.error.code.as_deref()) {
                (StatusCode::TOO_MANY_REQUESTS, _) => {
                    ApiError::RateLimit(error_response.error.message)
                }
                (StatusCode::UNAUTHORIZED, _) => {
                    ApiError::Authentication(error_response.error.message)
                }
                (StatusCode::BAD_REQUEST, _) => {
                    ApiError::InvalidRequest(error_response.error.message)
                }
                (status, _) if status.is_server_error() => {
                    ApiError::ServiceError(error_response.error.message)
                }
                _ => ApiError::Unknown(error_response.error.message),
            }
        } else {
            ApiError::Unknown(format!("Status {}: {}", status, response_text))
        };

        Err(ApiErrorContext {
            error,
            rate_limits: Some(rate_limits),
        }
        .into())
    }

    async fn try_send_request(
        &self,
        request: &OpenAIRequest,
    ) -> Result<(LLMResponse, OpenAIRateLimitInfo)> {
        let request = request.clone().into_non_streaming();
        let response = self
            .client
            .post(&self.get_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| ApiError::NetworkError(e.to_string()))?;

        let response = Self::check_response_error(response).await?;
        let rate_limits = OpenAIRateLimitInfo::from_response(&response);

        let response_text = response
            .text()
            .await
            .map_err(|e| ApiError::NetworkError(e.to_string()))?;

        // Parse the successful response
        let openai_response: OpenAIResponse = serde_json::from_str(&response_text)
            .map_err(|e| ApiError::Unknown(format!("Failed to parse response: {}", e)))?;

        // Convert to our generic LLMResponse format
        Ok((
            LLMResponse {
                content: {
                    let mut blocks = Vec::new();

                    // Add text content if present
                    if !openai_response.choices[0].message.content.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: openai_response.choices[0].message.content.clone(),
                        });
                    }

                    // Add tool calls if present
                    if let Some(ref tool_calls) = openai_response.choices[0].message.tool_calls {
                        for call in tool_calls {
                            let input =
                                serde_json::from_str(&call.function.arguments).map_err(|e| {
                                    ApiError::Unknown(format!(
                                        "Failed to parse tool arguments: {}",
                                        e
                                    ))
                                })?;
                            blocks.push(ContentBlock::ToolUse {
                                id: call.id.clone(),
                                name: call.function.name.clone(),
                                input,
                            });
                        }
                    }

                    blocks
                },
                usage: Usage {
                    input_tokens: openai_response.usage.prompt_tokens,
                    output_tokens: openai_response.usage.completion_tokens,
                },
            },
            rate_limits,
        ))
    }

    async fn try_send_request_streaming(
        &self,
        request: &OpenAIRequest,
        streaming_callback: &StreamingCallback,
    ) -> Result<(LLMResponse, OpenAIRateLimitInfo)> {
        debug!("Sending streaming request");
        let request = request.clone().into_streaming();
        let response = self
            .client
            .post(&self.get_url())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| ApiError::NetworkError(e.to_string()))?;

        let mut response = Self::check_response_error(response).await?;

        let mut accumulated_content: Option<String> = None;
        let mut accumulated_tool_calls: Vec<ContentBlock> = Vec::new();
        let mut current_tool: Option<OpenAIToolCallDelta> = None;

        let mut line_buffer = String::new();
        let mut usage = None;

        fn process_chunk(
            chunk: &[u8],
            line_buffer: &mut String,
            accumulated_content: &mut Option<String>,
            current_tool: &mut Option<OpenAIToolCallDelta>,
            accumulated_tool_calls: &mut Vec<ContentBlock>,
            callback: &StreamingCallback,
            usage: &mut Option<OpenAIUsage>,
        ) -> Result<()> {
            let chunk_str = std::str::from_utf8(chunk)?;

            for c in chunk_str.chars() {
                if c == '\n' {
                    if !line_buffer.is_empty() {
                        process_sse_line(
                            line_buffer,
                            accumulated_content,
                            current_tool,
                            accumulated_tool_calls,
                            callback,
                            usage,
                        )?;
                        line_buffer.clear();
                    }
                } else {
                    line_buffer.push(c);
                }
            }
            Ok(())
        }

        fn process_sse_line(
            line: &str,
            accumulated_content: &mut Option<String>,
            current_tool: &mut Option<OpenAIToolCallDelta>,
            accumulated_tool_calls: &mut Vec<ContentBlock>,
            callback: &StreamingCallback,
            usage: &mut Option<OpenAIUsage>,
        ) -> Result<()> {
            if let Some(data) = line.strip_prefix("data: ") {
                // Skip "[DONE]" message
                if data == "[DONE]" {
                    return Ok(());
                }

                if let Ok(chunk_response) = serde_json::from_str::<OpenAIStreamResponse>(data) {
                    if let Some(delta) = chunk_response.choices.get(0) {
                        // Handle content streaming
                        if let Some(content) = &delta.delta.content {
                            callback(content)?;
                            *accumulated_content = Some(
                                accumulated_content
                                    .as_ref()
                                    .unwrap_or(&String::new())
                                    .clone()
                                    + content,
                            );
                        }

                        // Handle tool calls
                        if let Some(tool_calls) = &delta.delta.tool_calls {
                            for tool_call in tool_calls {
                                if let Some(function) = &tool_call.function {
                                    if tool_call.id.is_some() {
                                        // New tool call
                                        if let Some(prev_tool) = current_tool.take() {
                                            accumulated_tool_calls
                                                .push(OpenAIClient::build_tool_block(prev_tool)?);
                                        }
                                        *current_tool = Some(tool_call.clone());
                                    } else if let Some(curr_tool) = current_tool {
                                        // Update existing tool
                                        if let Some(args) = &function.arguments {
                                            if let Some(ref mut curr_func) = curr_tool.function {
                                                curr_func.arguments = Some(
                                                    curr_func
                                                        .arguments
                                                        .as_ref()
                                                        .unwrap_or(&String::new())
                                                        .clone()
                                                        + args,
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Handle completion
                        if delta.finish_reason.is_some() {
                            if let Some(tool) = current_tool.take() {
                                accumulated_tool_calls.push(OpenAIClient::build_tool_block(tool)?);
                            }
                        }
                    }
                    // Capture usage data from final chunk
                    if let Some(chunk_usage) = chunk_response.usage {
                        *usage = Some(chunk_usage);
                    }
                }
            }
            Ok(())
        }

        while let Some(chunk) = response.chunk().await? {
            process_chunk(
                &chunk,
                &mut line_buffer,
                &mut accumulated_content,
                &mut current_tool,
                &mut accumulated_tool_calls,
                streaming_callback,
                &mut usage,
            )?;
        }

        // Process any remaining data in the buffer
        if !line_buffer.is_empty() {
            process_sse_line(
                &line_buffer,
                &mut accumulated_content,
                &mut current_tool,
                &mut accumulated_tool_calls,
                streaming_callback,
                &mut usage,
            )?;
        }

        let mut content = Vec::new();
        if let Some(text) = accumulated_content {
            content.push(ContentBlock::Text { text });
        }
        content.extend(accumulated_tool_calls);

        Ok((
            LLMResponse {
                content,
                usage: usage
                    .map(|u| Usage {
                        input_tokens: u.prompt_tokens,
                        output_tokens: u.completion_tokens,
                    })
                    .unwrap_or(Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    }),
            },
            OpenAIRateLimitInfo::from_response(&response),
        ))
    }

    fn build_tool_block(tool: OpenAIToolCallDelta) -> Result<ContentBlock> {
        let function = tool
            .function
            .ok_or_else(|| anyhow::anyhow!("Tool call without function"))?;
        let name = function
            .name
            .ok_or_else(|| anyhow::anyhow!("Function without name"))?;
        let args = function.arguments.unwrap_or_default();

        Ok(ContentBlock::ToolUse {
            id: tool.id.unwrap_or_default(),
            name,
            input: serde_json::from_str(&args)
                .map_err(|e| anyhow::anyhow!("Invalid JSON in arguments: {}", e))?,
        })
    }
}

#[async_trait]
impl LLMProvider for OpenAIClient {
    async fn send_message(
        &self,
        request: LLMRequest,
        streaming_callback: Option<&StreamingCallback>,
    ) -> Result<LLMResponse> {
        let mut messages: Vec<OpenAIChatMessage> = Vec::new();

        // Add system message
        messages.push(OpenAIChatMessage {
            role: "system".to_string(),
            content: request.system_prompt,
            tool_calls: None,
        });

        // Add conversation messages
        messages.extend(request.messages.iter().map(Self::convert_message));

        let openai_request = OpenAIRequest {
            model: self.model.clone(),
            messages,
            temperature: 1.0,
            stream: None,
            tool_choice: match &request.tools {
                Some(_) => Some(serde_json::json!("required")),
                _ => None,
            },
            tools: request.tools.map(|tools| {
                tools
                    .into_iter()
                    .map(|tool| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters
                            }
                        })
                    })
                    .collect()
            }),
            stream_options: None,
        };

        self.send_with_retry(&openai_request, streaming_callback, 3)
            .await
    }
}
