//! Tiny stdio MCP server used by integration tests. Two fake tools, no I/O.
//!
//! Run via `cargo run --example mock_server` for manual inspection.

use rmcp::ServiceExt;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Clone)]
struct MockServer {
    tool_router: ToolRouter<Self>,
}

impl MockServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AlphaParams {
    /// The thing to alpha.
    thing: String,
    /// How loudly to alpha.
    #[serde(default)]
    volume: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BetaParams {
    /// Targets to beta over.
    targets: Vec<String>,
}

#[tool_router]
impl MockServer {
    #[tool(
        name = "alpha_do",
        description = "Do an alpha thing with given volume."
    )]
    async fn alpha_do(&self, Parameters(p): Parameters<AlphaParams>) -> Result<String, ErrorData> {
        Ok(format!("alpha {} @ {:?}", p.thing, p.volume))
    }

    #[tool(name = "beta_run", description = "Run beta against a list of targets.")]
    async fn beta_run(&self, Parameters(p): Parameters<BetaParams>) -> Result<String, ErrorData> {
        Ok(format!("beta over {} targets", p.targets.len()))
    }
}

#[tool_handler]
impl ServerHandler for MockServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some("Mock server for librarian tests.".to_string()),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let srv = MockServer::new();
    let running = srv.serve((tokio::io::stdin(), tokio::io::stdout())).await?;
    running.waiting().await?;
    Ok(())
}
