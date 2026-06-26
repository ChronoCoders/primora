#![deny(warnings)]
//! primora-admin: operator CLI for driving mint proposals through the
//! MiningContract multi-sig lifecycle (propose -> approve -> execute) and
//! managing them against the Postgres proposal queue.
//!
//! Output convention: user-facing command results (the proposal table, tx
//! hashes, status fields) are printed with `println!` because that IS this
//! program's output. Internal/diagnostic logging uses `tracing`.
//!
//! Dual-chain routing (Decision 4c): each MiningContract is deployed per chain.
//! `propose` auto-routes from the chain stored on the Postgres proposal row;
//! `approve`/`status`/`execute`/`cancel` take an explicit `--chain` because a
//! proposal id alone does not encode which chain it lives on.

use std::str::FromStr;

use alloy::primitives::{hex, keccak256, Address};
use alloy::signers::local::PrivateKeySigner;
use clap::{Parser, Subcommand};
use common::{Chain, ProposalStatus, SessionId};
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
    #[arg(long, env = "ETHEREUM_RPC_URL", global = true)]
    ethereum_rpc_url: Option<String>,
    /// Ethereum MiningContract address.
    #[arg(long, env = "ETHEREUM_MINING_CONTRACT_ADDRESS", global = true)]
    ethereum_mining_address: Option<String>,
    /// Ethereum owner/proposer key (hex, with or without 0x).
    #[arg(long, env = "ETHEREUM_ADMIN_KEY_HEX", global = true)]
    ethereum_admin_key: Option<String>,
    /// Polygon JSON-RPC endpoint.
    #[arg(long, env = "POLYGON_RPC_URL", global = true)]
    polygon_rpc_url: Option<String>,
    /// Polygon MiningContract address.
    #[arg(long, env = "POLYGON_MINING_CONTRACT_ADDRESS", global = true)]
    polygon_mining_address: Option<String>,
    /// Polygon owner/proposer key (hex, with or without 0x).
    #[arg(long, env = "POLYGON_ADMIN_KEY_HEX", global = true)]
    polygon_admin_key: Option<String>,
    #[command(subcommand)]
    command: Command,
}

/// Admin subcommands.
#[derive(Subcommand)]
enum Command {
    /// List pending proposals from Postgres (read-only).
    List,
    /// Propose a mint for a pending session. Auto-routes to the chain recorded
    /// on the Postgres proposal row.
    Propose {
        /// Source session identifier.
        #[arg(long)]
        session_id: String,
    },
    /// Approve a pending on-chain proposal on the given chain.
    Approve {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
        /// Target chain: `ethereum` or `polygon`.
        #[arg(long)]
        chain: String,
    },
    /// Show on-chain proposal state and timelock remaining on the given chain.
    Status {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
        /// Target chain: `ethereum` or `polygon`.
        #[arg(long)]
        chain: String,
    },
    /// Execute a fully-approved proposal after its timelock on the given chain.
    Execute {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
        /// Target chain: `ethereum` or `polygon`.
        #[arg(long)]
        chain: String,
        /// Source session id, to mark the Postgres row Confirmed.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Cancel a pending proposal on the given chain.
    Cancel {
        /// Proposal id (32-byte hex).
        #[arg(long)]
        proposal_id: String,
        /// Target chain: `ethereum` or `polygon`.
        #[arg(long)]
        chain: String,
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

/// Parses a chain identifier (`ethereum` or `polygon`).
fn parse_chain(raw: &str) -> Result<Chain, String> {
    Chain::from_str_id(raw).ok_or_else(|| format!("invalid chain '{raw}' (expected ethereum or polygon)"))
}

/// Returns `value` or a descriptive error naming the flag and env var.
fn require(value: Option<String>, flag: &str, env: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("missing {flag} (or env {env})"))
}

/// Builds a signer-backed MiningContract writer for `chain` from that chain's
/// configured RPC, MiningContract address, and admin key. Errors clearly when
/// the chain is not configured.
async fn build_writer_for_chain(cli: &Cli, chain: Chain) -> Result<MiningWriter, String> {
    let (rpc, mining, key, prefix) = match chain {
        Chain::Ethereum => (
            cli.ethereum_rpc_url.clone(),
            cli.ethereum_mining_address.clone(),
            cli.ethereum_admin_key.clone(),
            "ETHEREUM",
        ),
        Chain::Polygon => (
            cli.polygon_rpc_url.clone(),
            cli.polygon_mining_address.clone(),
            cli.polygon_admin_key.clone(),
            "POLYGON",
        ),
    };
    let (rpc, mining, key) = match (rpc, mining, key) {
        (Some(rpc), Some(mining), Some(key)) => (rpc, mining, key),
        _ => {
            return Err(format!(
                "chain {chain} not configured (set {prefix}_RPC_URL, {prefix}_MINING_CONTRACT_ADDRESS, {prefix}_ADMIN_KEY_HEX)"
            ))
        }
    };
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
                "{:<38} {:<44} {:<26} {:<10} {:<9} {:<10} {}",
                "session_id", "wallet", "gross_prm", "commodity", "chain", "status", "proposal_id"
            );
            for row in &rows {
                let pid = derive_proposal_id(&row.session_id);
                println!(
                    "{:<38} {:<44} {:<26} {:<10} {:<9} {:<10} 0x{}",
                    row.session_id,
                    row.wallet,
                    row.gross_prm,
                    row.commodity,
                    row.chain,
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
            let chain = parse_chain(&row.chain)?;
            let recipient =
                Address::from_str(&row.wallet).map_err(|e| format!("bad wallet in db: {e}"))?;
            let amount: u128 = row
                .gross_prm
                .parse()
                .map_err(|_| format!("bad gross_prm in db: {}", row.gross_prm))?;
            let proposal_id = derive_proposal_id(session_id);
            let writer = build_writer_for_chain(&cli, chain).await?;
            let tx = writer
                .propose_mint(proposal_id, proposal_id, recipient, amount)
                .await
                .map_err(|e| e.to_string())?;
            store
                .update_proposal_status(&SessionId(session_id.clone()), ProposalStatus::Submitted)
                .await
                .map_err(|e| e.to_string())?;
            println!("proposalId: 0x{}", hex::encode(proposal_id));
            println!("chain:      {chain}");
            println!("recipient:  {recipient}");
            println!("amount:     {amount}");
            println!("tx:         {tx}");
        }
        Command::Approve { proposal_id, chain } => {
            let id = parse_proposal_id(proposal_id)?;
            let chain = parse_chain(chain)?;
            let writer = build_writer_for_chain(&cli, chain).await?;
            let tx = writer.approve_mint(id).await.map_err(|e| e.to_string())?;
            let proposal = writer.get_proposal(id).await.map_err(|e| e.to_string())?;
            println!("chain:     {chain}");
            println!("tx:        {tx}");
            println!("approvals: {}", proposal.approvals);
        }
        Command::Status { proposal_id, chain } => {
            let id = parse_proposal_id(proposal_id)?;
            let chain = parse_chain(chain)?;
            let writer = build_writer_for_chain(&cli, chain).await?;
            let proposal = writer.get_proposal(id).await.map_err(|e| e.to_string())?;
            if proposal.proposed_at == 0 {
                println!("proposal 0x{} not found on-chain ({chain})", hex::encode(id));
                return Ok(());
            }
            let unlock_at = proposal.proposed_at + TIMELOCK_SECS;
            let now = now_secs();
            println!("chain:       {chain}");
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
        Command::Execute { proposal_id, chain, session_id } => {
            let id = parse_proposal_id(proposal_id)?;
            let chain = parse_chain(chain)?;
            let writer = build_writer_for_chain(&cli, chain).await?;
            let tx = writer.execute_mint(id).await.map_err(|e| e.to_string())?;
            println!("chain: {chain}");
            println!("tx: {tx}");
            update_db_status(&cli, session_id.as_deref(), ProposalStatus::Confirmed).await?;
        }
        Command::Cancel { proposal_id, chain, session_id } => {
            let id = parse_proposal_id(proposal_id)?;
            let chain = parse_chain(chain)?;
            let writer = build_writer_for_chain(&cli, chain).await?;
            let tx = writer.cancel_mint(id).await.map_err(|e| e.to_string())?;
            println!("chain: {chain}");
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

    #[test]
    fn test_parse_chain() {
        assert_eq!(parse_chain("ethereum").unwrap(), Chain::Ethereum);
        assert_eq!(parse_chain("polygon").unwrap(), Chain::Polygon);
        assert!(parse_chain("solana").is_err());
        assert!(parse_chain("Ethereum").is_err());
    }
}
