use anyhow::{Context, Result};
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub database_readonly_url: String,
    pub qdrant_url: String,
    pub host: String,
    pub port: u16,

    // Vectorization settings
    pub vectorize_messages: bool,
    pub vectorize_wiki: bool,
    pub messages_vectorization_method: String,
    pub messages_vectorization_api_url: Option<String>,
    pub messages_vectorization_interval_secs: u64,
    pub messages_vectorization_protocol: String,
    pub messages_vectorization_api_key: Option<String>,
    pub messages_vectorization_api_model: Option<String>,
    pub wiki_vectorization_method: String,
    pub wiki_vectorization_api_url: Option<String>,
    pub wiki_vectorization_interval_secs: u64,
    pub wiki_vectorization_protocol: String,
    pub wiki_vectorization_api_key: Option<String>,
    pub wiki_vectorization_api_model: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
        let database_readonly_url = std::env::var("DATABASE_READONLY_URL")
            .unwrap_or_else(|_| database_url.clone());
        let qdrant_url =
            std::env::var("QDRANT_URL").unwrap_or_else(|_| "http://localhost:6333".to_string());
        let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = std::env::var("PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse()
            .context("PORT must be a valid number")?;

        let vectorize_messages = std::env::var("VECTORIZE_MESSAGES")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);
        let vectorize_wiki = std::env::var("VECTORIZE_WIKI")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);
        let messages_vectorization_method =
            std::env::var("MESSAGES_VECTORIZATION_METHOD").unwrap_or_else(|_| "local".to_string());
        let messages_vectorization_api_url = std::env::var("MESSAGES_VECTORIZATION_API_URL").ok();
        let messages_vectorization_interval_secs = std::env::var("MESSAGES_VECTORIZATION_INTERVAL")
            .unwrap_or_else(|_| "3600".to_string())
            .parse()
            .context("MESSAGES_VECTORIZATION_INTERVAL must be a valid number")?;
        let messages_vectorization_protocol = std::env::var("MESSAGES_VECTORIZATION_PROTOCOL")
            .unwrap_or_else(|_| "openai".to_string());
        let messages_vectorization_api_key = std::env::var("MESSAGES_VECTORIZATION_API_KEY").ok();
        let messages_vectorization_api_model =
            std::env::var("MESSAGES_VECTORIZATION_API_MODEL").ok();
        let wiki_vectorization_method =
            std::env::var("WIKI_VECTORIZATION_METHOD").unwrap_or_else(|_| "local".to_string());
        let wiki_vectorization_api_url = std::env::var("WIKI_VECTORIZATION_API_URL").ok();
        let wiki_vectorization_interval_secs = std::env::var("WIKI_VECTORIZATION_INTERVAL")
            .unwrap_or_else(|_| "3600".to_string())
            .parse()
            .context("WIKI_VECTORIZATION_INTERVAL must be a valid number")?;
        let wiki_vectorization_protocol =
            std::env::var("WIKI_VECTORIZATION_PROTOCOL").unwrap_or_else(|_| "openai".to_string());
        let wiki_vectorization_api_key = std::env::var("WIKI_VECTORIZATION_API_KEY").ok();
        let wiki_vectorization_api_model = std::env::var("WIKI_VECTORIZATION_API_MODEL").ok();

        Ok(Self {
            database_url,
            database_readonly_url,
            qdrant_url,
            host,
            port,
            vectorize_messages,
            vectorize_wiki,
            messages_vectorization_method,
            messages_vectorization_api_url,
            messages_vectorization_interval_secs,
            messages_vectorization_protocol,
            messages_vectorization_api_key,
            messages_vectorization_api_model,
            wiki_vectorization_method,
            wiki_vectorization_api_url,
            wiki_vectorization_interval_secs,
            wiki_vectorization_protocol,
            wiki_vectorization_api_key,
            wiki_vectorization_api_model,
        })
    }

    #[expect(dead_code)]
    pub fn socket_addr(&self) -> Result<SocketAddr> {
        let addr = format!("{}:{}", self.host, self.port);
        addr.parse().context("Invalid socket address")
    }
}
