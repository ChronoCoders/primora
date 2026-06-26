-- Dual-chain mint routing (Spec Decision 4c): record which chain each proposal
-- mints to. DEFAULT 'ethereum' backfills any pre-dual-chain rows, which were all
-- single-chain Ethereum, so the migration is safe on existing data.
ALTER TABLE mint_proposals ADD COLUMN chain TEXT NOT NULL DEFAULT 'ethereum';
CREATE INDEX IF NOT EXISTS idx_mint_proposals_chain ON mint_proposals(chain);
