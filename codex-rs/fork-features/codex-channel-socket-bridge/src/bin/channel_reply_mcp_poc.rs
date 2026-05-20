use rmcp::ErrorData as McpError;
use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use serde::Deserialize;
use serde_json::json;
use std::borrow::Cow;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task;

const TOOL_NAME: &str = "channel_reply";

#[derive(Clone)]
struct ChannelReplyServer {
    log_path: Option<PathBuf>,
    tools: Arc<Vec<Tool>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelReplyArgs {
    target: String,
    text: String,
}

impl ChannelReplyServer {
    fn new(log_path: Option<PathBuf>) -> Result<Self, serde_json::Error> {
        Ok(Self {
            log_path,
            tools: Arc::new(vec![channel_reply_tool()?]),
        })
    }

    fn append_call_log(&self, args: &ChannelReplyArgs) -> Result<(), McpError> {
        let Some(log_path) = &self.log_path else {
            return Ok(());
        };
        if let Some(parent) = log_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let line = json!({
            "tool": TOOL_NAME,
            "target": args.target,
            "text": args.text,
        });
        writeln!(file, "{line}").map_err(|err| McpError::internal_error(err.to_string(), None))
    }
}

impl ServerHandler for ChannelReplyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
            ..ServerInfo::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = Arc::clone(&self.tools);
        async move {
            Ok(ListToolsResult {
                tools: (*tools).clone(),
                next_cursor: None,
                meta: None,
            })
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if request.name != TOOL_NAME {
            return Err(McpError::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        }
        let value = serde_json::Value::Object(
            request
                .arguments
                .unwrap_or_default()
                .into_iter()
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        );
        let args: ChannelReplyArgs = serde_json::from_value(value)
            .map_err(|err| McpError::invalid_params(err.to_string(), None))?;
        self.append_call_log(&args)?;
        Ok(CallToolResult {
            content: vec![Content::text(format!("delivered to {}", args.target))],
            structured_content: Some(json!({
                "ok": true,
                "delivered_to": args.target,
            })),
            is_error: Some(false),
            meta: None,
        })
    }
}

fn channel_reply_tool() -> Result<Tool, serde_json::Error> {
    let schema: JsonObject = serde_json::from_value(json!({
        "type": "object",
        "required": ["target", "text"],
        "properties": {
            "target": {
                "type": "string",
                "description": "Channel target id from the inbound message (format: <transport>:<id>, e.g. telegram:12345)"
            },
            "text": {
                "type": "string",
                "description": "Reply text to deliver to the external recipient"
            }
        },
        "additionalProperties": false
    }))?;

    Ok(Tool::new(
        Cow::Borrowed(TOOL_NAME),
        Cow::Borrowed(
            "Send a reply to an external channel recipient. The inbound channel message you are responding to tells you what `target` to use. Calling this tool is the ONLY way to deliver text to the external party -- your normal assistant text is invisible to them.",
        ),
        Arc::new(schema),
    ))
}

fn parse_log_path() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--log" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

fn stdio() -> (tokio::io::Stdin, tokio::io::Stdout) {
    (tokio::io::stdin(), tokio::io::stdout())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("starting issue-5 channel_reply MCP PoC server");
    let service = ChannelReplyServer::new(parse_log_path())?;
    let running = service.serve(stdio()).await?;
    running.waiting().await?;
    task::yield_now().await;
    Ok(())
}
