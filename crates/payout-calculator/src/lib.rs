#![deny(warnings)]
#![deny(missing_docs)]
//! Spec Section 4.6 payout formula: gross PRM, USD redemption, house edge, and
//! staking boost, computed entirely in scaled `u128` integers.

use common::Commodity;

/// Scale factor applied to commodity multipliers (e.g. `32` means `3.2x`).
const MULTIPLIER_SCALE: u128 = 10;
/// Scale factor applied to `price_coefficient_scaled` (`1e15`).
const PRICE_COEFFICIENT_SCALE: u128 = 1_000_000_000_000_000;
/// Scale factor of an 8-decimal Chainlink/Pyth price.
const TWAP_DECIMALS_SCALE: u128 = 100_000_000;
/// Scale factor of a 6-decimal USDC amount.
const USDC_SCALE: u128 = 1_000_000;
/// Basis-point denominator (`10_000` bps = `100%`).
const BPS_DENOMINATOR: u128 = 10_000;

/// Factor converting a calibration-scaled gross PRM value (human PRM x 10^5, the
/// internal redemption-input scale produced by [`calculate_gross_prm`]) to ERC-20
/// base units (human PRM x 10^18). `10^18 / 10^5 = 10^13`.
pub const GROSS_CALIB_TO_WEI: u128 = 10_000_000_000_000;

/// Converts a calibration-scaled gross PRM value to ERC-20 base units (wei).
/// `gross_calib` is human PRM x 10^5; the result is human PRM x 10^18. Uses
/// checked multiplication, saturating to `u128::MAX` on overflow (guarded, though
/// realistic sessions stay far inside `u128`).
pub fn gross_calib_to_wei(gross_calib: u128) -> u128 {
    gross_calib.saturating_mul(GROSS_CALIB_TO_WEI)
}

/// Fixed PRM reference price in USD, scaled to 8 decimals (Spec 4.8).
/// `$0.10` per PRM = `10_000_000`. Used for all PRM->USD conversions until PRM
/// trades on a market, at which point this is replaced by the real price.
pub const PRM_REFERENCE_PRICE_8DEC: u128 = 10_000_000;

/// Divisor rescaling an 8-decimal USD value to cents (2 decimals): `10^(8-2)`.
const USD_8DEC_TO_CENTS: u128 = 1_000_000;

/// Converts a human PRM amount (whole PRM units as `u128`) to USD cents at the
/// fixed reference price [`PRM_REFERENCE_PRICE_8DEC`] (Spec 4.8). Integer math
/// only; no float.
///
/// This is the reference-price *valuation* of PRM (`$0.10` per PRM), reserved for
/// the reserve-ratio "circulating PRM value in USD" input. It is NOT the earnings
/// figure: earnings report net commodity-redemption USD (`SUM(net_usd_cents)`),
/// a different quantity. The input is human PRM (i.e. base-unit wei divided by
/// `10^18`), never a calibration-scaled or wei value.
///
/// Scale: `prm_amount` (human PRM) * [`PRM_REFERENCE_PRICE_8DEC`] yields USD at
/// 8-decimal precision (`prm * $0.10`); dividing by [`USD_8DEC_TO_CENTS`]
/// (`10^6`) rescales that to cents. Examples: `1000 PRM -> $100.00 -> 10_000`
/// cents; `148 PRM -> $14.80 -> 1_480` cents; `0 -> 0`.
pub fn prm_to_usd_cents(prm_amount: u128) -> u128 {
    prm_amount * PRM_REFERENCE_PRICE_8DEC / USD_8DEC_TO_CENTS
}

/// Calibration constants for the payout formula.
#[derive(Debug, Clone)]
pub struct PayoutConfig {
    /// Base coefficient scaled by 1_000_000 (e.g. 1000 = 0.001).
    pub base_coefficient_scaled: u128,
    /// Price coefficient scaled by 1_000_000_000_000_000 (1e15).
    pub price_coefficient_scaled: u128,
    /// House edge in basis points (1700 = 17%).
    pub house_edge_bps: u128,
}

/// Returns the calibrated payout configuration.
///
/// `price_coefficient_scaled` is calibrated against the Spec Section 4.6 worked
/// example (Desktop, 8 hours, Gold): hashrate 2500 H/s, duration 28800s, Gold
/// TWAP $3204 (`320_400_000_000` at 8 decimals), house edge 1700 bps. At
/// `price_coefficient_scaled = 1_000` this yields redemption `$18.455040` and
/// net `$15.317683`, reproducing the spec's illustrative `$18.45` / `$15.31`
/// and landing inside the `$15-22` target band. The `examples/calibrate.rs`
/// sweep shows the adjacent candidates fall outside the band (`$1.53` at 100,
/// `$153.18` at 10_000).
///
/// Final production calibration is confirmed by pre-launch stress testing
/// across all client types and commodities (Spec Open Decision #12); this
/// default is the stress-test starting point.
pub fn default_config() -> PayoutConfig {
    PayoutConfig {
        base_coefficient_scaled: 1_000,
        price_coefficient_scaled: 1_000,
        house_edge_bps: 1_700,
    }
}

/// Returns the redemption multiplier for `commodity`, scaled by 10.
pub fn commodity_multiplier(commodity: &Commodity) -> u128 {
    match commodity {
        Commodity::Gold => 32,
        Commodity::Platinum => 26,
        Commodity::Silver => 16,
        Commodity::CrudeOil => 10,
    }
}

/// Returns the mining difficulty for `commodity`, scaled by 10.
pub fn commodity_difficulty(commodity: &Commodity) -> u128 {
    match commodity {
        Commodity::Gold => 40,
        Commodity::Platinum => 32,
        Commodity::Silver => 20,
        Commodity::CrudeOil => 10,
    }
}

/// Computes gross PRM for a session (Spec Section 4.6, Step 1).
pub fn calculate_gross_prm(
    hashrate: u64,
    duration_secs: u64,
    config: &PayoutConfig,
    commodity: &Commodity,
) -> u128 {
    let numerator =
        hashrate as u128 * duration_secs as u128 * config.base_coefficient_scaled;
    numerator / commodity_difficulty(commodity)
}

/// Computes the redemption value in USD for `gross_prm` at `twap_price` (Spec
/// Section 4.6, Step 2), returned scaled to 6 decimals (USDC format).
pub fn calculate_redemption_usd(
    gross_prm: u128,
    twap_price: u128,
    commodity: &Commodity,
    config: &PayoutConfig,
) -> u128 {
    let multiplier = commodity_multiplier(commodity);
    gross_prm
        * twap_price
        * multiplier
        * config.price_coefficient_scaled
        * USDC_SCALE
        / MULTIPLIER_SCALE
        / PRICE_COEFFICIENT_SCALE
        / TWAP_DECIMALS_SCALE
}

/// Applies the house edge to a 6-decimal USDC amount (Spec Section 4.6, Step 3).
pub fn apply_house_edge(redemption_usd_scaled: u128, house_edge_bps: u128) -> u128 {
    redemption_usd_scaled * (BPS_DENOMINATOR - house_edge_bps) / BPS_DENOMINATOR
}

/// Applies a staking boost to gross PRM. `boost_bps` is 0 for no boost, up to
/// 4000 (40%) per spec.
pub fn apply_staking_boost(gross_prm: u128, boost_bps: u32) -> u128 {
    gross_prm * (BPS_DENOMINATOR + boost_bps as u128) / BPS_DENOMINATOR
}

/// One PRM token in wei (18 decimals), used for the staking tier thresholds.
const PRM: u128 = 1_000_000_000_000_000_000;
/// Hard cap on the effective staking boost (40%), mirroring StakingContract.sol.
pub const MAX_BOOST_BPS: u32 = 4_000;
/// Lock-multiplier scale factor (a multiplier of `130` means 1.3x).
const LOCK_MULT_SCALE: u32 = 100;

/// Returns the base staking boost in basis points for a total staked amount,
/// matching the deployed `StakingContract.baseBoostBps` tier table (Spec 6.4).
/// `total_staked` is in PRM wei (18 decimals).
pub fn base_boost_bps(total_staked: u128) -> u32 {
    if total_staked >= 500_000 * PRM {
        2_500
    } else if total_staked >= 100_000 * PRM {
        1_800
    } else if total_staked >= 50_000 * PRM {
        1_000
    } else if total_staked >= 10_000 * PRM {
        500
    } else {
        0
    }
}

/// Returns the lock multiplier scaled by 100 for an enum ordinal lock period
/// (0=30d, 1=90d, 2=180d), matching `StakingContract.lockMultiplier` (Spec 6.5).
/// Any other ordinal defaults to 1.0x.
pub fn lock_multiplier_scaled(lock_period: u8) -> u32 {
    match lock_period {
        0 => 100,
        1 => 130,
        2 => 160,
        _ => 100,
    }
}

/// Computes the combined cross-chain effective staking boost in basis points
/// (Spec Section 6.4/6.5).
///
/// `ethereum_stake` is `Some((amount, lock_period))` for an active Ethereum
/// stake; `polygon_stake` is `Some(amount)` for an active Polygon stake. Amounts
/// are PRM wei (18 decimals). Steps:
/// 1. `total` = Ethereum amount + Polygon amount (tier driven by the combined
///    cross-chain total).
/// 2. `base` = [`base_boost_bps`]`(total)`.
/// 3. `lock_mult` = the Ethereum lock multiplier if there is an active Ethereum
///    stake, otherwise 1.0x. Polygon never contributes a lock multiplier (its
///    stored lock period is intentionally ignored; Polygon is always 1.0x).
/// 4. `effective = base * lock_mult / 100`, capped at [`MAX_BOOST_BPS`].
pub fn combined_boost_bps(
    ethereum_stake: Option<(u128, u8)>,
    polygon_stake: Option<u128>,
) -> u32 {
    let ethereum_amount = ethereum_stake.map(|(amount, _)| amount).unwrap_or(0);
    let polygon_amount = polygon_stake.unwrap_or(0);
    let total = ethereum_amount.saturating_add(polygon_amount);
    let base = base_boost_bps(total);
    let lock_mult = match ethereum_stake {
        Some((_, lock_period)) => lock_multiplier_scaled(lock_period),
        None => LOCK_MULT_SCALE,
    };
    let effective = base * lock_mult / LOCK_MULT_SCALE;
    effective.min(MAX_BOOST_BPS)
}

/// Full payout breakdown for a session.
#[derive(Debug, Clone)]
pub struct PayoutResult {
    /// Gross PRM minted (no decimals applied yet).
    pub gross_prm: u128,
    /// Redemption value in USD scaled to 6 decimals (USDC format).
    pub redemption_usd_scaled: u128,
    /// Net USDC after house edge, scaled to 6 decimals.
    pub net_usdc_scaled: u128,
    /// House edge applied in basis points.
    pub house_edge_bps: u128,
}

/// Computes the full payout: gross PRM, USD redemption, and net USDC after the
/// configured house edge.
pub fn calculate_payout(
    hashrate: u64,
    duration_secs: u64,
    twap_price: u128,
    commodity: &Commodity,
    config: &PayoutConfig,
) -> PayoutResult {
    let gross_prm = calculate_gross_prm(hashrate, duration_secs, config, commodity);
    calculate_payout_from_gross(gross_prm, twap_price, commodity, config)
}

/// Computes the payout from a pre-computed `gross_prm`, skipping the gross
/// derivation. Used when the gross already reflects a staking boost (Spec
/// 6.5/4.6): the caller computes gross via [`calculate_gross_prm`], boosts it
/// with [`apply_staking_boost`], then runs redemption and house edge here.
pub fn calculate_payout_from_gross(
    gross_prm: u128,
    twap_price: u128,
    commodity: &Commodity,
    config: &PayoutConfig,
) -> PayoutResult {
    let redemption_usd_scaled = calculate_redemption_usd(gross_prm, twap_price, commodity, config);
    let net_usdc_scaled = apply_house_edge(redemption_usd_scaled, config.house_edge_bps);
    PayoutResult {
        gross_prm,
        redemption_usd_scaled,
        net_usdc_scaled,
        house_edge_bps: config.house_edge_bps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prm_to_usd_cents() {
        assert_eq!(prm_to_usd_cents(1000), 10_000);
        assert_eq!(prm_to_usd_cents(148), 1_480);
        assert_eq!(prm_to_usd_cents(0), 0);
    }

    #[test]
    fn test_gross_calib_to_wei() {
        assert_eq!(gross_calib_to_wei(1_800_000_000), 18_000 * 10u128.pow(18));
        assert_eq!(gross_calib_to_wei(187_500), 1_875 * 10u128.pow(15));
        assert_eq!(gross_calib_to_wei(0), 0);
    }

    #[test]
    fn test_gross_prm_gold_8h() {
        let gross = calculate_gross_prm(2500, 28800, &default_config(), &Commodity::Gold);
        assert_eq!(gross, 1_800_000_000);
    }

    #[test]
    fn test_redemption_gold() {
        let redemption = calculate_redemption_usd(
            1_800_000_000,
            320_400_000_000,
            &Commodity::Gold,
            &default_config(),
        );
        assert_eq!(redemption, 18_455_040);
    }

    #[test]
    fn test_house_edge_17pct() {
        assert_eq!(apply_house_edge(1_000_000, 1_700), 830_000);
    }

    #[test]
    fn test_staking_boost_25pct() {
        assert_eq!(apply_staking_boost(1_000_000, 2_500), 1_250_000);
    }

    #[test]
    fn test_staking_boost_max_40pct() {
        assert_eq!(apply_staking_boost(1_000_000, 4_000), 1_400_000);
    }

    #[test]
    fn test_calculate_payout_returns_struct() {
        let result = calculate_payout(
            2500,
            28800,
            320_400_000_000,
            &Commodity::Gold,
            &default_config(),
        );
        assert_eq!(result.gross_prm, 1_800_000_000);
        assert_eq!(result.redemption_usd_scaled, 18_455_040);
        assert_eq!(result.net_usdc_scaled, 15_317_683);
        assert_eq!(result.house_edge_bps, 1_700);
        assert!(result.net_usdc_scaled < result.redemption_usd_scaled);
    }

    #[test]
    fn test_gold_8h_in_target_range() {
        let result = calculate_payout(
            2500,
            28800,
            320_400_000_000,
            &Commodity::Gold,
            &default_config(),
        );
        assert!(
            (15_000_000..=22_000_000).contains(&result.net_usdc_scaled),
            "net {} outside the $15-22 target band",
            result.net_usdc_scaled
        );
    }

    const E18: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn test_base_boost_tiers() {
        assert_eq!(base_boost_bps(9_999 * E18), 0);
        assert_eq!(base_boost_bps(10_000 * E18), 500);
        assert_eq!(base_boost_bps(50_000 * E18), 1_000);
        assert_eq!(base_boost_bps(100_000 * E18), 1_800);
        assert_eq!(base_boost_bps(500_000 * E18), 2_500);
    }

    #[test]
    fn test_lock_multiplier() {
        assert_eq!(lock_multiplier_scaled(0), 100);
        assert_eq!(lock_multiplier_scaled(1), 130);
        assert_eq!(lock_multiplier_scaled(2), 160);
        assert_eq!(lock_multiplier_scaled(7), 100);
    }

    #[test]
    fn test_boost_polygon_only_10k() {
        assert_eq!(combined_boost_bps(None, Some(10_000 * E18)), 500);
    }

    #[test]
    fn test_boost_combined_60k_eth90d() {
        assert_eq!(
            combined_boost_bps(Some((30_000 * E18, 1)), Some(30_000 * E18)),
            1_300
        );
    }

    #[test]
    fn test_boost_eth_100k_180d() {
        assert_eq!(combined_boost_bps(Some((100_000 * E18, 2)), None), 2_880);
    }

    #[test]
    fn test_boost_eth_500k_180d_caps() {
        assert_eq!(combined_boost_bps(Some((500_000 * E18, 2)), None), 4_000);
    }

    #[test]
    fn test_boost_below_min() {
        assert_eq!(combined_boost_bps(None, Some(5_000 * E18)), 0);
    }

    #[test]
    fn test_apply_staking_boost_integration() {
        let boost = combined_boost_bps(Some((30_000 * E18, 1)), Some(30_000 * E18));
        assert_eq!(boost, 1_300);
        assert_eq!(apply_staking_boost(18_000, boost), 20_340);
    }
}
