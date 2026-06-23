CREATE TABLE IF NOT EXISTS anomaly_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  TEXT NOT NULL,
    wallet      TEXT NOT NULL,
    score       INTEGER NOT NULL DEFAULT 0,
    triggers    JSONB NOT NULL DEFAULT '[]',
    level       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_anomaly_events_session_id ON anomaly_events(session_id);
CREATE INDEX IF NOT EXISTS idx_anomaly_events_wallet ON anomaly_events(wallet);
CREATE INDEX IF NOT EXISTS idx_anomaly_events_level ON anomaly_events(level);
