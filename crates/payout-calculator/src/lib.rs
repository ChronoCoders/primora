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
}
