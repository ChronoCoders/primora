-- Spec 4.8 USD wiring: persist the net payout in USD cents (redemption minus
-- house edge) per proposal. Nullable: rows created before this migration predate
-- USD persistence and remain NULL, for which the frontend renders a dash. No
-- backfill is attempted because the historical net USD cannot be reconstructed
-- without each session's TWAP at payout time.
ALTER TABLE mint_proposals ADD COLUMN net_usd_cents BIGINT;
