mod agent;
mod explorer;
mod llm;
mod types;
mod ui;
mod utils;

use crate::agent::Agent;
use crate::explorer::Explorer;
use crate::llm::{AnthropicClient, LLMProvider, OpenAIClient};
use crate::ui::terminal::TerminalUI;
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tracing::Level;

/// AI-powered coding assistant
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the code directory to analyze
    #[arg(short, long, default_value = ".")]
    path: PathBuf,

    /// Task to perform on the codebase
    #[arg(short, long)]
    task: String,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

fn create_llm_client() -> Result<Box<dyn LLMProvider>> {
    // Try Anthropic first
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        return Ok(Box::new(AnthropicClient::new(
            api_key,
            "claude-3-5-sonnet-20241022".to_string(),
        )));
    }

    // Try OpenAI as fallback
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        return Ok(Box::new(OpenAIClient::new(
            api_key,
            "gpt-4o-latest".to_string(),
        )));
    }

    // No API keys available
    anyhow::bail!(
        "Neither ANTHROPIC_API_KEY nor OPENAI_API_KEY environment variables are set. \
                  Please set at least one of them to use the code assistant."
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command line arguments
    let args = Args::parse();

    // Setup logging based on verbose flag
    let log_level = if args.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .pretty()
        .init();

    // Ensure the path exists and is a directory
    if !args.path.is_dir() {
        anyhow::bail!("Path '{}' is not a directory", args.path.display());
    }

    // Setup LLM client - try providers in order of preference
    let llm_client = create_llm_client().context("Failed to initialize LLM client")?;

    // Setup CodeExplorer
    let root_path = args.path.canonicalize()?;
    let explorer = Box::new(Explorer::new(root_path));

    // Initialize terminal UI
    let terminal_ui = Box::new(TerminalUI::new());

    // Initialize agent
    let mut agent = Agent::new(llm_client, explorer, terminal_ui);

    // Start agent with the specified task
    agent.start(args.task).await?;

    Ok(())
}
