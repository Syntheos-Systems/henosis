//! `agent-forge`: the thin CLI wrapper over `agent-forge-lib` (Phase 2 Story 2.1).
//!
//! The CLI contract is byte-compatible with the upstream Kleos-repo binary: every invocation
//! names a subcommand, reads a JSON input file, runs one tool against the on-disk SQLite forge
//! DB, and writes the JSON `Output` envelope back, so external hooks keep working unchanged.
//! All tool logic lives in the library; this file only parses arguments, does the file IO, and
//! wires the HTTP skills bridge (`KLEOS_URL` / `KLEOS_API_KEY`).

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use agent_forge_lib::bridge::HttpSkillsBridge;
use agent_forge_lib::json_io::{read_input, write_output, Output};
use agent_forge_lib::{run_tool, Database, SkillsBridge, Tool};

/// Top-level CLI: every invocation specifies a subcommand plus input/output JSON paths.
#[derive(Parser)]
#[command(name = "agent-forge")]
#[command(about = "Structured reasoning and code quality workflow")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to input JSON file
    #[arg(long)]
    input: PathBuf,

    /// Path to output JSON file
    #[arg(long)]
    output: PathBuf,

    /// Path to database file
    #[arg(long, default_value = "~/.agent-forge/forge.db")]
    db: String,
}

/// One enum variant per agent-forge tool. Names map 1:1 to the agent-forge tool reference
/// (and to [`Tool`]; clap needs its own derive-friendly enum).
#[derive(Subcommand, Debug)]
enum Commands {
    SpecTask,
    ConsiderApproaches,
    LogHypothesis,
    LogOutcome,
    RecallErrors,
    Verify,
    ChallengeCode,
    CommentCheck,
    Checkpoint,
    Rollback,
    SessionLearn,
    SessionRecall,
    SessionDiff,
    Think,
    DeclareUnknowns,
    UpdateSpec,
    ListSpecs,
    GetSpec,
    Stats,
    RepoMap,
    SearchCode,
    SkillSearch,
    SkillCapture,
    SkillRecordExec,
    SkillFix,
    SkillDerive,
    SkillLineage,
}

/// Map the clap subcommand onto the library's [`Tool`].
fn tool_for(command: &Commands) -> Tool {
    match command {
        Commands::SpecTask => Tool::SpecTask,
        Commands::ConsiderApproaches => Tool::ConsiderApproaches,
        Commands::LogHypothesis => Tool::LogHypothesis,
        Commands::LogOutcome => Tool::LogOutcome,
        Commands::RecallErrors => Tool::RecallErrors,
        Commands::Verify => Tool::Verify,
        Commands::ChallengeCode => Tool::ChallengeCode,
        Commands::CommentCheck => Tool::CommentCheck,
        Commands::Checkpoint => Tool::Checkpoint,
        Commands::Rollback => Tool::Rollback,
        Commands::SessionLearn => Tool::SessionLearn,
        Commands::SessionRecall => Tool::SessionRecall,
        Commands::SessionDiff => Tool::SessionDiff,
        Commands::Think => Tool::Think,
        Commands::DeclareUnknowns => Tool::DeclareUnknowns,
        Commands::UpdateSpec => Tool::UpdateSpec,
        Commands::ListSpecs => Tool::ListSpecs,
        Commands::GetSpec => Tool::GetSpec,
        Commands::Stats => Tool::Stats,
        Commands::RepoMap => Tool::RepoMap,
        Commands::SearchCode => Tool::SearchCode,
        Commands::SkillSearch => Tool::SkillSearch,
        Commands::SkillCapture => Tool::SkillCapture,
        Commands::SkillRecordExec => Tool::SkillRecordExec,
        Commands::SkillFix => Tool::SkillFix,
        Commands::SkillDerive => Tool::SkillDerive,
        Commands::SkillLineage => Tool::SkillLineage,
    }
}

/// Expand a leading `~/` in a path string to the user's home directory.
fn expand_path(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Parse args, open the forge DB, dispatch through the library, and write the JSON result to
/// `--output`. Any error becomes an `Output::error` payload (the upstream contract).
fn main() {
    let cli = Cli::parse();

    let db_path = expand_path(&cli.db);
    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            let output = Output::error(format!("Database error: {e}"));
            write_output(&cli.output, &output).ok();
            std::process::exit(1);
        }
    };

    // The bridge is best-effort: an unconstructible HTTP client degrades to bridgeless mode,
    // where skill tools answer with the standard error envelope (upstream parity).
    let bridge = HttpSkillsBridge::from_env().ok();
    let bridge_ref = bridge.as_ref().map(|b| b as &dyn SkillsBridge);

    let output = match read_input::<serde_json::Value>(&cli.input) {
        Ok(input) => run_tool(&db, tool_for(&cli.command), input, bridge_ref),
        Err(e) => Output::error(e.to_string()),
    };

    if let Err(e) = write_output(&cli.output, &output) {
        eprintln!("Failed to write output: {e}");
        std::process::exit(1);
    }
}
