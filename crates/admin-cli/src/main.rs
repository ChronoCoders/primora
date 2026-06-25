#![deny(warnings)]
//! primora-admin: operator CLI for driving mint proposals through the
//! MiningContract multi-sig lifecycle (propose -> approve -> execute) and
//! managing them against the Postgres proposal queue.
//!
//! Output convention: user-facing command results (the proposal table, tx
//! hashes, status fields) are printed with `println!` because that IS this
//! program's output. Internal/diagnostic logging uses `tracing`.

use std::str::FromStr;

use alloy::primitives::{hex, keccak256, Address};
use alloy::signers::local::PrivateKeySigner;
use clap::{Parser, Subcommand};
use common::{ProposalStatus, SessionId};
use onchain_client::MiningWriter;
use postgres_store::PostgresStore;

const TIMELOCK_SECS: u64 = 48 * 60 * 60;

/// Operator CLI for the Primora mint pipeline.
#[derive(Parser)]
#[command(name = "primora-admin", version, about)]
struct Cli {
    /// PostgreSQL connection string.
    #[arg(long, env = "DATABASE_URL", global = true)]
    database_url: Option<String>,
    /// Ethereum JSON-RPC endpoint.
    #[arg(long, env = "RPC_URL", global = true)]
    rpc_url: Option<String>,
    /// Deployed MiningContract address.
    #[arg(long, env = "MINING_CONTRACT_ADDRESS", global = true)]
    mining_address: Option<String>,
    /// Owner/proposer key (hex, with or without 0x).
    #[arg(long, env = "ADMIN_KEY_HEX", global = true)]
    admin_key: Option<String>,
    #[command(subcommand)]
    command: Command,
}

/// Admin subcommands.
#[derive(Subcommand)]
enum Command {
    /// List pending proposals from Postgres (read-only).
    List,
    /// Propose a mint for a pending session.
    Propose {
        /// Source session identifier.
        #[arg(long)]
        session_id: String,
    },
    /// Approve a pending on-chain proposal.
    Approve {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
    },
    /// Show on-chain proposal state and timelock remaining.
    Status {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
    },
    /// Execute a fully-approved proposal after its timelock.
    Execute {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
        /// Source session id, to mark the Postgres row Confirmed.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Cancel a pending proposal.
    Cancel {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
        /// Source session id, to mark the Postgres row Rejected.
        #[arg(long)]
        session_id: Option<String>,
    },
}

/// Parses a 32-byte proposal id from hex, with or without a `0x` prefix.
fn parse_proposal_id(raw: &str) -> Result<[u8; 32], String> {
    let stripped = raw.strip_prefix("0x").unwrap_or(raw);
    let bytes = hex::decode(stripped).map_err(|e| format!("invalid proposal-id hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| "proposal-id must be exactly 32 bytes".to_string())
}

/// Derives a deterministic proposal id from a session id string.
fn derive_proposal_id(session_id: &str) -> [u8; 32] {
    keccak256(session_id.as_bytes()).0
}

/// Returns `value` or a descriptive error naming the flag and env var.
fn require(value: Option<String>, flag: &str, env: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("missing {flag} (or env {env})"))
}

/// Builds a signer-backed MiningContract writer from the global options.
async fn build_writer(cli: &Cli) -> Result<MiningWriter, String> {
    let rpc = require(cli.rpc_url.clone(), "--rpc-url", "RPC_URL")?;
    let mining = require(cli.mining_address.clone(), "--mining-address", "MINING_CONTRACT_ADDRESS")?;
    let key = require(cli.admin_key.clone(), "--admin-key", "ADMIN_KEY_HEX")?;
    let address = Address::from_str(&mining).map_err(|e| format!("invalid mining address: {e}"))?;
    let signer = PrivateKeySigner::from_str(&key).map_err(|e| format!("invalid admin key: {e}"))?;
    MiningWriter::new(&rpc, signer, address)
        .await
        .map_err(|e| e.to_string())
}

/// Builds a Postgres store from the global options.
async fn build_store(cli: &Cli) -> Result<PostgresStore, String> {
    let db = require(cli.database_url.clone(), "--database-url", "DATABASE_URL")?;
    PostgresStore::new(&db).await.map_err(|e| e.to_string())
}

/// Returns the current Unix time in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn run(cli: Cli) -> Result<(), String> {
    match &cli.command {
        Command::List => {
            let store = build_store(&cli).await?;
            let rows = store.get_pending_proposals().await.map_err(|e| e.to_string())?;
            println!(
                "{:<38} {:<44} {:<26} {:<10} {:<10} {}",
                "session_id", "wallet", "gross_prm", "commodity", "status", "proposal_id"
            );
            for row in &rows {
                let pid = derive_proposal_id(&row.session_id);
                println!(
                    "{:<38} {:<44} {:<26} {:<10} {:<10} 0x{}",
                    row.session_id,
                    row.wallet,
                    row.gross_prm,
                    row.commodity,
                    row.status,
                    hex::encode(pid)
                );
            }
            println!("({} pending)", rows.len());
        }
        Command::Propose { session_id } => {
            let store = build_store(&cli).await?;
            let rows = store.get_pending_proposals().await.map_err(|e| e.to_string())?;
            let row = rows
                .into_iter()
                .find(|r| &r.session_id == session_id)
                .ok_or_else(|| format!("no pending proposal for session {session_id}"))?;
            let recipient =
                Address::from_str(&row.wallet).map_err(|e| format!("bad wallet in db: {e}"))?;
            let amount: u128 = row
                .gross_prm
                .parse()
                .map_err(|_| format!("bad gross_prm in db: {}", row.gross_prm))?;
            let proposal_id = derive_proposal_id(session_id);
            let writer = build_writer(&cli).await?;
            let tx = writer
                .propose_mint(proposal_id, proposal_id, recipient, amount)
                .await
                .map_err(|e| e.to_string())?;
            store
                .update_proposal_status(&SessionId(session_id.clone()), ProposalStatus::Submitted)
                .await
                .map_err(|e| e.to_string())?;
            println!("proposalId: 0x{}", hex::encode(proposal_id));
            println!("recipient:  {recipient}");
            println!("amount:     {amount}");
            println!("tx:         {tx}");
        }
        Command::Approve { proposal_id } => {
            let id = parse_proposal_id(proposal_id)?;
            let writer = build_writer(&cli).await?;
            let tx = writer.approve_mint(id).await.map_err(|e| e.to_string())?;
            let proposal = writer.get_proposal(id).await.map_err(|e| e.to_string())?;
            println!("tx:        {tx}");
            println!("approvals: {}", proposal.approvals);
        }
        Command::Status { proposal_id } => {
            let id = parse_proposal_id(proposal_id)?;
            let writer = build_writer(&cli).await?;
            let proposal = writer.get_proposal(id).await.map_err(|e| e.to_string())?;
            if proposal.proposed_at == 0 {
                println!("proposal 0x{} not found on-chain", hex::encode(id));
                return Ok(());
            }
            let unlock_at = proposal.proposed_at + TIMELOCK_SECS;
            let now = now_secs();
            println!("session_id:  0x{}", hex::encode(proposal.session_id));
            println!("recipient:   {}", proposal.recipient);
            println!("amount:      {}", proposal.amount);
            println!("proposed_at: {}", proposal.proposed_at);
            println!("approvals:   {}", proposal.approvals);
            println!("executed:    {}", proposal.executed);
            println!("cancelled:   {}", proposal.cancelled);
            if now >= unlock_at {
                println!("timelock:    ELAPSED (executable)");
            } else {
                println!("timelock:    {} seconds remaining", unlock_at - now);
            }
        }
        Command::Execute { proposal_id, session_id } => {
            let id = parse_proposal_id(proposal_id)?;
            let writer = build_writer(&cli).await?;
            let tx = writer.execute_mint(id).await.map_err(|e| e.to_string())?;
            println!("tx: {tx}");
            update_db_status(&cli, session_id.as_deref(), ProposalStatus::Confirmed).await?;
        }
        Command::Cancel { proposal_id, session_id } => {
            let id = parse_proposal_id(proposal_id)?;
            let writer = build_writer(&cli).await?;
            let tx = writer.cancel_mint(id).await.map_err(|e| e.to_string())?;
            println!("tx: {tx}");
            update_db_status(&cli, session_id.as_deref(), ProposalStatus::Rejected).await?;
        }
    }
    Ok(())
}

/// Updates the Postgres proposal row for `session_id` when provided. A
/// proposal id cannot be reversed to a session id, so the operator must pass
/// `--session-id` for the database row to be updated; otherwise the on-chain
/// action stands alone and the row is left unchanged.
async fn update_db_status(
    cli: &Cli,
    session_id: Option<&str>,
    status: ProposalStatus,
) -> Result<(), String> {
    match session_id {
        Some(sid) => {
            let store = build_store(cli).await?;
            store
                .update_proposal_status(&SessionId(sid.to_string()), status)
                .await
                .map_err(|e| e.to_string())?;
            println!("postgres: session {sid} marked {status:?}");
        }
        None => {
            println!("postgres: not updated (pass --session-id to mark the row {status:?})");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proposal_id_hex() {
        let bare = "1111111111111111111111111111111111111111111111111111111111111111";
        let prefixed = format!("0x{bare}");
        assert_eq!(
            parse_proposal_id(bare).unwrap(),
            parse_proposal_id(&prefixed).unwrap()
        );
    }

    #[test]
    fn test_parse_proposal_id_rejects_short() {
        assert!(parse_proposal_id("0x1234").is_err());
    }

    #[test]
    fn test_derive_proposal_id() {
        let a = derive_proposal_id("session-xyz");
        let b = derive_proposal_id("session-xyz");
        assert_eq!(a, b);
        assert_ne!(a, [0u8; 32]);
        assert_ne!(derive_proposal_id("session-xyz"), derive_proposal_id("other"));
    }
}
