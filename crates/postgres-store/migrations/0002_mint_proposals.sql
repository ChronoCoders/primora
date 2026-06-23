CREATE TABLE IF NOT EXISTS mint_proposals (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id          TEXT NOT NULL UNIQUE,
    wallet              TEXT NOT NULL,
    gross_prm           NUMERIC(78, 0) NOT NULL,
    commodity           TEXT NOT NULL,
    attestation_json    JSONB NOT NULL,
    backend_sig         TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'Pending',
    created_at          TIMESTAMPTZ NOT NULL,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_mint_proposals_wallet ON mint_proposals(wallet);
CREATE INDEX IF NOT EXISTS idx_mint_proposals_status ON mint_proposals(status);
