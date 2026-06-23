# Integration Tests

Requires:
- Redis: set REDIS_URL=redis://localhost:6379
- Postgres: set DATABASE_URL=postgres://primora:primora_dev@localhost:5432/primora
- Service (optional): set SERVICE_URL=http://localhost:3000

Run all:
  cargo test -p integration-tests

Run with live services (docker compose up -d first):
  REDIS_URL=redis://localhost:6379 \
  DATABASE_URL=postgres://primora:primora_dev@localhost:5432/primora \
  SERVICE_URL=http://localhost:3000 \
  cargo test -p integration-tests

Tests that require external services skip gracefully if env vars are not set.
