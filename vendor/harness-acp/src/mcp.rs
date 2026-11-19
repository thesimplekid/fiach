//! Authenticated, task-scoped MCP bridge for application-owned tools.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, JsonObject, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Application tools. Implementations must confine filesystem and side effects
/// themselves; the bridge only exposes this explicit catalog.
#[async_trait]
pub trait ToolHandler: Send + Sync + 'static {
    /// Fixed tool catalog for this task.
    fn tools(&self) -> Vec<Tool>;
    /// Dispatch one advertised call, obeying cancellation.
    async fn call(
        &self,
        name: &str,
        arguments: Option<JsonObject>,
        cancel: CancellationToken,
    ) -> CallToolResult;
}

#[derive(Clone)]
struct Handler {
    tools: Arc<Vec<Tool>>,
    inner: Arc<dyn ToolHandler>,
    validators: Arc<HashMap<String, jsonschema::Validator>>,
    cancel: CancellationToken,
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: self.tools.as_ref().clone(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::invalid_params(
                "Tool is not permitted for this task",
                None,
            ));
        }
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        if let Some(validator) = self.validators.get(request.name.as_ref())
            && !validator.is_valid(&arguments)
        {
            return Err(ErrorData::invalid_params(
                "Tool arguments do not match the schema",
                None,
            ));
        }
        let cancel = self.cancel.child_token();
        let result = tokio::select! {
            _ = context.ct.cancelled() => {
                cancel.cancel();
                CallToolResult::error(vec![rmcp::model::ContentBlock::text("Tool call cancelled")])
            }
            _ = self.cancel.cancelled() => {
                cancel.cancel();
                CallToolResult::error(vec![rmcp::model::ContentBlock::text("Task cancelled")])
            }
            result = self.inner.call(&request.name, request.arguments, cancel.clone()) => result,
        };
        cancel.cancel();
        Ok(result.into())
    }
}

async fn authorize(
    State(token): State<Arc<String>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let supplied = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok());
    if supplied != Some(token.as_str()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    Ok(next
        .run(Request::from_parts(parts, axum::body::Body::from(body)))
        .await)
}

/// An in-process MCP server bound exclusively to loopback. Dropping it stops
/// accepting calls and cancels active tool handlers.
pub struct McpBridge {
    url: String,
    authorization: Arc<String>,
    tools: Vec<String>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl McpBridge {
    /// Start a fresh bridge with a random bearer credential and fixed catalog.
    pub async fn start(
        inner: Arc<dyn ToolHandler>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let tools = inner.tools();
        let mut names = HashSet::new();
        for tool in &tools {
            if tool.name.is_empty() || !names.insert(tool.name.to_string()) {
                return Err("MCP tool names must be nonempty and unique".into());
            }
        }
        let mut validators = HashMap::new();
        for tool in &tools {
            validators.insert(
                tool.name.to_string(),
                jsonschema::validator_for(&Value::Object(tool.input_schema.as_ref().clone()))
                    .map_err(|error| error.to_string())?,
            );
        }
        let names = tools.iter().map(|tool| tool.name.to_string()).collect();
        let cancel = CancellationToken::new();
        let handler = Handler {
            tools: Arc::new(tools),
            inner,
            validators: Arc::new(validators),
            cancel: cancel.clone(),
        };
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(cancel.child_token());
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let authorization = Arc::new(format!("Bearer {}", uuid::Uuid::new_v4()));
        let router = axum::Router::new().nest_service("/mcp", service).layer(
            middleware::from_fn_with_state(authorization.clone(), authorize),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await;
        });
        Ok(Self {
            url,
            authorization,
            tools: names,
            cancel,
            task,
        })
    }

    /// Goose recipe extension descriptor. Contains a credential: do not persist
    /// or log this value with the task transcript.
    pub fn extension(&self, name: &str) -> Value {
        json!({"type":"streamable_http", "name":name, "uri":self.url,
            "headers":{"Authorization":self.authorization.as_str()},
            "available_tools":self.tools, "timeout":300})
    }
}

impl Drop for McpBridge {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Probe(Arc<AtomicUsize>);

    #[async_trait]
    impl ToolHandler for Probe {
        fn tools(&self) -> Vec<Tool> {
            vec![Tool::new(
                "inspect",
                "Read only",
                rmcp::object!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            )]
        }
        async fn call(
            &self,
            _: &str,
            _: Option<JsonObject>,
            _: CancellationToken,
        ) -> CallToolResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            CallToolResult::success(vec![rmcp::model::ContentBlock::text("evidence")])
        }
    }

    async fn post(bridge: &McpBridge, authorized: bool, body: Value) -> String {
        let address = bridge
            .url
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/mcp")
            .unwrap();
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let body = body.to_string();
        let auth = if authorized {
            format!("Authorization: {}\r\n", bridge.authorization)
        } else {
            String::new()
        };
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {address}\r\n{auth}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2025-03-26\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        response
    }

    #[tokio::test]
    async fn bridge_rejects_unauthorized_unknown_and_invalid_calls() {
        let count = Arc::new(AtomicUsize::new(0));
        let bridge = McpBridge::start(Arc::new(Probe(count.clone())))
            .await
            .unwrap();
        let call = |name, args| json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
        assert!(
            post(&bridge, false, call("inspect", json!({"path":"a"})))
                .await
                .contains("401")
        );
        assert!(
            post(&bridge, true, call("shell", json!({"path":"a"})))
                .await
                .contains("not permitted")
        );
        assert!(
            post(&bridge, true, call("inspect", json!({"path":7})))
                .await
                .contains("do not match")
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(
            post(&bridge, true, call("inspect", json!({"path":"a"})))
                .await
                .contains("evidence")
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
