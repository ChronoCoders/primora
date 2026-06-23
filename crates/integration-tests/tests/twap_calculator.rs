use chrono::{Duration, Utc};
use twap_calculator::{OracleSource, PriceSample, TwapCalculator};

fn sample(price: u128, at: chrono::DateTime<Utc>) -> PriceSample {
    PriceSample {
        price,
        sampled_at: at,
        oracle: OracleSource::Chainlink,
    }
}

#[test]
fn test_full_session_flow() {
    let session_start = Utc::now() - Duration::minutes(15);
    let mut calc = TwapCalculator::new(session_start);
    calc.add_sample(sample(320_000_000_000, session_start + Duration::minutes(5)));
    calc.add_sample(sample(320_400_000_000, session_start + Duration::minutes(10)));
    calc.add_sample(sample(320_800_000_000, session_start + Duration::minutes(15)));

    assert_eq!(calc.calculate(), Some(320_400_000_000));
    assert!(calc.is_valid());
    assert_eq!(calc.sample_count(), 3);

    let Some(result) = calc.finalize(Utc::now()) else {
        panic!("finalize returned None");
    };
    assert!(result.is_valid);
}

#[test]
fn test_short_session_invalid() {
    let session_start = Utc::now() - Duration::minutes(5);
    let mut calc = TwapCalculator::new(session_start);
    calc.add_sample(sample(320_000_000_000, session_start + Duration::minutes(4)));

    assert!(!calc.is_valid());

    let Some(result) = calc.finalize(Utc::now()) else {
        panic!("finalize returned None");
    };
    assert!(!result.is_valid);
}
