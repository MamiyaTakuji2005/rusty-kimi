use std::collections::HashMap;
use std::time::Duration;

use reqwest::header::HeaderMap;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::constant::user_agent;
use crate::soul::toolset::get_current_tool_call_or_none;
use crate::tools::SkipThisTool;
use crate::tools::utils::{DEFAULT_MAX_CHARS, ToolResultBuilder, load_desc};

use kosong::tooling::{CallableTool2, ToolReturnValue};

const SEARCH_DESC: &str = include_str!("../desc/web/search.md");

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    #[schemars(description = "The query text to search for.")]
    pub query: String,
    #[serde(default = "default_search_limit")]
    #[schemars(
        description = "The optional maximum returned results.",
        range(min = 1, max = 20),
        default = "default_search_limit"
    )]
    pub limit: i64,
}

fn default_search_limit() -> i64 {
    5
}

#[derive(Clone, Deserialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    #[serde(default)]
    date: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    search_results: Vec<SearchResult>,
}

pub struct SearchWeb {
    description: String,
    base_url: String,
    api_key: String,
    custom_headers: HashMap<String, String>,
}

impl SearchWeb {
    pub fn new(runtime: &crate::soul::agent::Runtime) -> Result<Self, SkipThisTool> {
        let service = runtime
            .config
            .services
            .moonshot_search
            .clone()
            .ok_or(SkipThisTool)?;
        let desc = load_desc(SEARCH_DESC, &[]);
        Ok(Self {
            description: desc,
            base_url: service.base_url,
            api_key: service.api_key,
            custom_headers: service.custom_headers.unwrap_or_default(),
        })
    }
}

#[async_trait::async_trait]
impl CallableTool2 for SearchWeb {
    type Params = SearchParams;

    fn name(&self) -> &str {
        "SearchWeb"
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn call_typed(&self, params: Self::Params) -> ToolReturnValue {
        let mut builder = ToolResultBuilder::new(DEFAULT_MAX_CHARS, None);

        if self.base_url.is_empty() || self.api_key.is_empty() {
            return builder.error(
                "Search service is not configured. You may want to try other methods to search.",
                "Search service not configured",
            );
        }

        let tool_call = match get_current_tool_call_or_none() {
            Some(call) => call,
            None => {
                return builder.error(
                    "Search service is not available without tool call context.",
                    "Search unavailable",
                );
            }
        };

        let mut headers = HeaderMap::new();
        if let Err(err) = insert_header(&mut headers, reqwest::header::USER_AGENT, &user_agent()) {
            return builder.error(
                &format!("Invalid user agent header: {err}"),
                "Invalid header",
            );
        }
        if let Err(err) = insert_header(
            &mut headers,
            reqwest::header::AUTHORIZATION,
            &format!("Bearer {}", self.api_key),
        ) {
            return builder.error(
                &format!("Invalid authorization header: {err}"),
                "Invalid header",
            );
        }
        if let Err(err) = insert_header(&mut headers, "X-Msh-Tool-Call-Id", &tool_call.id) {
            return builder.error(
                &format!("Invalid tool call id header: {err}"),
                "Invalid header",
            );
        }
        for (key, value) in &self.custom_headers {
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                value.parse(),
            ) {
                headers.insert(name, val);
            }
        }

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                return builder.error(
                    &format!("Failed to build HTTP client: {err}"),
                    "HTTP client error",
                );
            }
        };
        let resp = match client
            .post(&self.base_url)
            .headers(headers)
            .json(&serde_json::json!({
                "text_query": params.query,
                "limit": params.limit,
                "timeout_seconds": 30,
            }))
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                return builder.error(
                    &format!(
                        "Failed to search. Error: {err}. This may indicates that the search service is currently unavailable."
                    ),
                    "Failed to search",
                )
            }
        };

        if resp.status() != reqwest::StatusCode::OK {
            return builder.error(
                &format!(
                    "Failed to search. Status: {}. This may indicates that the search service is currently unavailable.",
                    resp.status()
                ),
                "Failed to search",
            );
        }

        let payload: SearchResponse = match resp.json().await {
            Ok(payload) => payload,
            Err(err) => {
                return builder.error(
                    &format!(
                        "Failed to parse search results. Error: {err}. This may indicates that the search service is currently unavailable."
                    ),
                    "Failed to parse search results",
                )
            }
        };

        for (idx, result) in payload.search_results.into_iter().enumerate() {
            if idx > 0 {
                builder.write("---\n\n");
            }
            builder.write(&format!(
                "Title: {}\nDate: {}\nURL: {}\nSummary: {}\n\n",
                result.title, result.date, result.url, result.snippet
            ));
        }

        builder.ok("", "")
    }
}

fn insert_header(
    headers: &mut HeaderMap,
    name: impl AsRef<str>,
    value: &str,
) -> Result<(), String> {
    let name = reqwest::header::HeaderName::from_bytes(name.as_ref().as_bytes())
        .map_err(|err| format!("invalid header name: {err}"))?;
    let val = value
        .parse::<reqwest::header::HeaderValue>()
        .map_err(|err| format!("invalid header value: {err}"))?;
    headers.insert(name, val);
    Ok(())
}
