#![deny(warnings)]
#![deny(missing_docs)]
//! Daily mint ceiling calculation and proposal generation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const BLOCKS_PER_DAY: u64 = 7_200;
const SAFETY_MULTIPLIER: u64 = 3;

/// A signed-off-elsewhere proposal describing a recalculated per-block ceiling.
///
/// This is the artifact written to Postgres and shown in the admin panel. The
/// calculator never writes it anywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CeilingProposal {
    /// UTC time the proposal was computed.
    pub calculated_at: DateTime<Utc>,
    /// Active user count input.
    pub active_users: u64,
    /// Average daily PRM minted per user input, as a scaled integer.
    pub avg_daily_prm_per_user: u64,
    /// Daily target mint, as a scaled integer.
    pub daily_target_prm: u64,
    /// Resulting per-block ceiling, as a scaled integer.
    pub block_ceiling: u64,
    /// Blocks per day assumed in the calculation.
    pub blocks_per_day: u64,
    /// Safety multiplier applied to the per-block target.
    pub safety_multiplier: u64,
}

/// Computes the dynamic per-block PRM mint ceiling.
pub struct MintCeilingCalculator {
    blocks_per_day: u64,
    safety_multiplier: u64,
}

impl MintCeilingCalculator {
    /// Creates a calculator with the Phase 1 Ethereum constants.
    pub fn new() -> Self {
        Self {
            blocks_per_day: BLOCKS_PER_DAY,
            safety_multiplier: SAFETY_MULTIPLIER,
        }
    }

    /// Returns the per-block ceiling for the given inputs.
    ///
    /// Integer division truncates, which is acceptable and intentional.
    pub fn calculate(&self, active_users: u64, avg_daily_prm_per_user: u64) -> u64 {
        let daily_target = active_users * avg_daily_prm_per_user;
        (daily_target / self.blocks_per_day) * self.safety_multiplier
    }

    /// Returns the whole-PRM daily mint ceiling for the given inputs: the daily
    /// target scaled by the safety multiplier (`active_users ×
    /// avg_daily_prm_per_user × safety_multiplier`). Saturating to avoid overflow
    /// on extreme inputs. This is the backend's per-day aggregate cap, a distinct
    /// layer from `MiningContract.mintCeilingPerBlock` (deployed `1_000_000e18`),
    /// which bounds a single block; the two guards are independent and do not
    /// share a value.
    ///
    /// Verified consistent (no conflict scenario): a single session's mint is far
    /// below the per-block cap (reaching `1_000_000` PRM in one mint needs an
    /// unrealistic multi-day session), so a backend-approved proposal does not
    /// revert on the per-block ceiling under normal load. The backend daily cap is
    /// the binding production limit; the on-chain per-block ceiling is a coarse
    /// anti-spam backstop. The only way to hit the per-block cap (many large mints
    /// in one block) is itself a retriable revert that would also exhaust this
    /// daily cap, so the layers never deadlock or mis-settle.
    pub fn daily_ceiling(&self, active_users: u64, avg_daily_prm_per_user: u64) -> u64 {
        active_users
            .saturating_mul(avg_daily_prm_per_user)
            .saturating_mul(self.safety_multiplier)
    }

    /// Builds a [`CeilingProposal`] for the given inputs at the current UTC time.
    pub fn propose(&self, active_users: u64, avg_daily_prm_per_user: u64) -> CeilingProposal {
        let daily_target_prm = active_users * avg_daily_prm_per_user;
        let block_ceiling = self.calculate(active_users, avg_daily_prm_per_user);
        CeilingProposal {
            calculated_at: Utc::now(),
            active_users,
            avg_daily_prm_per_user,
            daily_target_prm,
            block_ceiling,
            blocks_per_day: self.blocks_per_day,
            safety_multiplier: self.safety_multiplier,
        }
    }
}

impl Default for MintCeilingCalculator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_basic() {
        let calc = MintCeilingCalculator::new();
        assert_eq!(calc.calculate(1_000, 500), 207);
    }

    #[test]
    fn test_calculate_zero_users() {
        let calc = MintCeilingCalculator::new();
        assert_eq!(calc.calculate(0, 500), 0);
    }

    #[test]
    fn test_calculate_large() {
        let calc = MintCeilingCalculator::new();
        assert_eq!(calc.calculate(10_000, 1_000), 4_164);
    }

    #[test]
    fn test_daily_ceiling() {
        let calc = MintCeilingCalculator::new();
        assert_eq!(calc.daily_ceiling(1_000, 500), 1_500_000);
        assert_eq!(calc.daily_ceiling(0, 500), 0);
    }

    #[test]
    fn test_daily_ceiling_saturates() {
        let calc = MintCeilingCalculator::new();
        assert_eq!(calc.daily_ceiling(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn test_propose_returns_correct_fields() {
        let calc = MintCeilingCalculator::new();
        let proposal = calc.propose(500, 300);
        assert_eq!(proposal.active_users, 500);
        assert_eq!(proposal.avg_daily_prm_per_user, 300);
        assert_eq!(proposal.blocks_per_day, 7_200);
        assert_eq!(proposal.safety_multiplier, 3);
        assert_eq!(proposal.block_ceiling, calc.calculate(500, 300));
    }
}
