//! Calibration sweep for the payout formula's `price_coefficient_scaled`.
//!
//! Run with: `cargo run -p payout-calculator --example calibrate`
//!
//! Plugs the Spec Section 4.6 worked example (Desktop, 8 hours, Gold) into the
//! payout formula and sweeps candidate price coefficients, printing the
//! resulting net USDC so the value landing in the $15-22 target band can be
//! chosen. All math is integer; dollar figures are formatted from the 6-decimal
//! USDC-scaled integers for readability only.

use common::Commodity;
use payout_calculator::{
    apply_house_edge, calculate_gross_prm, calculate_redemption_usd, PayoutConfig,
};

/// Formats a 6-decimal USDC-scaled integer as a dollar string.
fn fmt_usdc(scaled: u128) -> String {
    format!("${}.{:06}", scaled / 1_000_000, scaled % 1_000_000)
}

fn main() {
    const HASHRATE: u64 = 2_500;
    const DURATION_SECS: u64 = 28_800;
    const TWAP_PRICE: u128 = 320_400_000_000;
    const HOUSE_EDGE_BPS: u128 = 1_700;
    const BAND_LOW: u128 = 15_000_000;
    const BAND_HIGH: u128 = 22_000_000;
    let commodity = Commodity::Gold;

    let candidates: [u128; 5] = [10, 100, 1_000, 10_000, 100_000];

    println!("Payout calibration sweep -- Desktop, 8h, Gold");
    println!(
        "inputs: hashrate={HASHRATE} H/s, duration={DURATION_SECS}s, twap={TWAP_PRICE} (8-dec), house_edge={HOUSE_EDGE_BPS} bps"
    );
    println!(
        "target net band: {} - {}",
        fmt_usdc(BAND_LOW),
        fmt_usdc(BAND_HIGH)
    );
    println!();
    println!(
        "{:>14}  {:>16}  {:>16}  {:>16}  {}",
        "price_coeff", "gross_prm", "redemption_usd", "net_usdc", "in_band"
    );

    for pc in candidates {
        let config = PayoutConfig {
            base_coefficient_scaled: 1_000,
            price_coefficient_scaled: pc,
            house_edge_bps: HOUSE_EDGE_BPS,
        };
        let gross = calculate_gross_prm(HASHRATE, DURATION_SECS, &config, &commodity);
        let redemption = calculate_redemption_usd(gross, TWAP_PRICE, &commodity, &config);
        let net = apply_house_edge(redemption, HOUSE_EDGE_BPS);
        let in_band = (BAND_LOW..=BAND_HIGH).contains(&net);
        println!(
            "{:>14}  {:>16}  {:>16}  {:>16}  {}",
            pc,
            gross,
            fmt_usdc(redemption),
            fmt_usdc(net),
            if in_band { "<== TARGET" } else { "" }
        );
    }
}
