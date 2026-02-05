use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{Quota, RateLimiter, clock::DefaultClock, state::{InMemoryState, NotKeyed}};
use serde::Deserialize;

use crate::config::{AiConfig, AiProvider};

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

#[async_trait::async_trait]
pub trait AiProviderTrait: Send + Sync {
    async fn generate(&self, system: &str, user: &str) -> anyhow::Result<String>;
}

pub fn create_provider(config: &AiConfig) -> anyhow::Result<Box<dyn AiProviderTrait>> {
    let quota = Quota::per_second(NonZeroU32::new(1).unwrap());
    let limiter = Arc::new(RateLimiter::direct(quota));
    let http = reqwest::Client::new();

    match config.provider {
        AiProvider::OpenAi => {
            let api_key = config.api_key.as_deref()
                .ok_or_else(|| anyhow::anyhow!("api_key required for OpenAI provider"))?;
            let base_url = config.base_url.as_deref()
                .unwrap_or("https://api.openai.com/v1");
            let model = config.model.as_deref().unwrap_or("gpt-4o");
            Ok(Box::new(OpenAiProvider {
                http,
                limiter,
                api_key: api_key.to_string(),
                base_url: base_url.to_string(),
                model: model.to_string(),
            }))
        }
        AiProvider::Anthropic => {
            let api_key = config.api_key.as_deref()
                .ok_or_else(|| anyhow::anyhow!("api_key required for Anthropic provider"))?;
            let base_url = config.base_url.as_deref()
                .unwrap_or("https://api.anthropic.com/v1");
            let model = config.model.as_deref().unwrap_or("claude-sonnet-4-20250514");
            Ok(Box::new(AnthropicProvider {
                http,
                limiter,
                api_key: api_key.to_string(),
                base_url: base_url.to_string(),
                model: model.to_string(),
            }))
        }
        AiProvider::Ollama => {
            let base_url = config.base_url.as_deref()
                .unwrap_or("http://localhost:11434");
            let model = config.model.as_deref().unwrap_or("llama3.1");
            Ok(Box::new(OllamaProvider {
                http,
                limiter,
                base_url: base_url.to_string(),
                model: model.to_string(),
            }))
        }
    }
}

// --- OpenAI ---

struct OpenAiProvider {
    http: reqwest::Client,
    limiter: Arc<Limiter>,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Deserialize)]
struct OpenAiResponsesResponse {
    output: Vec<OpenAiOutputItem>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum OpenAiOutputItem {
    #[serde(rename = "message")]
    Message { content: Vec<OpenAiContent> },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct OpenAiContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[async_trait::async_trait]
impl AiProviderTrait for OpenAiProvider {
    async fn generate(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.limiter.until_ready().await;

        let body = serde_json::json!({
            "model": self.model,
            "instructions": system,
            "input": user,
            "tools": [{ "type": "web_search" }],
        });

        let resp = self.http
            .post(format!("{}/responses", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error {}: {}", status, text);
        }

        let data: OpenAiResponsesResponse = resp.json().await?;
        data.output.into_iter()
            .find_map(|item| match item {
                OpenAiOutputItem::Message { content } => {
                    content.into_iter()
                        .find(|c| c.content_type == "output_text")
                        .and_then(|c| c.text)
                }
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("empty response from OpenAI"))
    }
}

// --- Anthropic ---

struct AnthropicProvider {
    http: reqwest::Client,
    limiter: Arc<Limiter>,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: String,
}

#[async_trait::async_trait]
impl AiProviderTrait for AnthropicProvider {
    async fn generate(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.limiter.until_ready().await;

        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 1024,
            "system": system,
            "messages": [
                { "role": "user", "content": user },
            ],
        });

        let resp = self.http
            .post(format!("{}/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error {}: {}", status, text);
        }

        let data: AnthropicResponse = resp.json().await?;
        data.content.into_iter().next()
            .map(|c| c.text)
            .ok_or_else(|| anyhow::anyhow!("empty response from Anthropic"))
    }
}

// --- Ollama ---

struct OllamaProvider {
    http: reqwest::Client,
    limiter: Arc<Limiter>,
    base_url: String,
    model: String,
}

#[derive(Deserialize)]
struct OllamaResponse {
    message: OllamaMessage,
}

#[derive(Deserialize)]
struct OllamaMessage {
    content: String,
}

#[async_trait::async_trait]
impl AiProviderTrait for OllamaProvider {
    async fn generate(&self, system: &str, user: &str) -> anyhow::Result<String> {
        self.limiter.until_ready().await;

        let body = serde_json::json!({
            "model": self.model,
            "stream": false,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });

        let resp = self.http
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error {}: {}", status, text);
        }

        let data: OllamaResponse = resp.json().await?;
        Ok(data.message.content)
    }
}
