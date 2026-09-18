-- Public, reconstructable chain index only. Never store credentials, raw signed transactions or local snapshots.
CREATE SCHEMA IF NOT EXISTS d20dao_explorer;
CREATE TABLE IF NOT EXISTS d20dao_explorer.deployments (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, registry TEXT NOT NULL,
 configuration JSONB NOT NULL, catalog JSONB NOT NULL, protocol_configuration_hash TEXT NOT NULL,
 implementation_pins JSONB NOT NULL, first_block BIGINT NOT NULL,
 last_indexed_block BIGINT, last_chain_timestamp BIGINT, canonical BOOLEAN NOT NULL DEFAULT TRUE,
 PRIMARY KEY(chain_id,coordinator)
);
CREATE TABLE IF NOT EXISTS d20dao_explorer.cursors (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, next_block BIGINT NOT NULL,
 PRIMARY KEY(chain_id,coordinator)
);
CREATE TABLE IF NOT EXISTS d20dao_explorer.blocks (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, number BIGINT NOT NULL, hash TEXT NOT NULL, timestamp BIGINT NOT NULL,
 PRIMARY KEY(chain_id,coordinator,number)
);
CREATE TABLE IF NOT EXISTS d20dao_explorer.epochs (
 chain_id TEXT NOT NULL, registry TEXT NOT NULL, epoch_id TEXT NOT NULL,
 record JSONB NOT NULL, packet TEXT NOT NULL CHECK(length(packet)<=4098), commit_timestamp BIGINT NOT NULL,
 block_number BIGINT NOT NULL, block_hash TEXT NOT NULL, tx_hash TEXT NOT NULL, log_index BIGINT NOT NULL,
 receipt JSONB NOT NULL, canonical BOOLEAN NOT NULL DEFAULT TRUE, PRIMARY KEY(chain_id,registry,epoch_id)
);
CREATE TABLE IF NOT EXISTS d20dao_explorer.requests (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, request_id TEXT NOT NULL,
 request JSONB NOT NULL, mapping JSONB NOT NULL, packet TEXT CHECK(packet IS NULL OR length(packet)=834),
 request_timestamp BIGINT NOT NULL, request_block BIGINT NOT NULL, request_block_hash TEXT NOT NULL,
 request_tx_hash TEXT NOT NULL, request_log_index BIGINT NOT NULL, request_receipt JSONB NOT NULL,
 fulfillment_timestamp BIGINT, fulfillment_block BIGINT, fulfillment_block_hash TEXT,
 fulfillment_tx_hash TEXT, fulfillment_log_index BIGINT, fulfillment_receipt JSONB,
 observed_block BIGINT NOT NULL, canonical BOOLEAN NOT NULL DEFAULT TRUE,
 PRIMARY KEY(chain_id,coordinator,request_id)
);
CREATE TABLE IF NOT EXISTS d20dao_explorer.events (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, address TEXT NOT NULL, block_number BIGINT NOT NULL,
 block_hash TEXT NOT NULL, tx_hash TEXT NOT NULL, log_index BIGINT NOT NULL, topic TEXT NOT NULL,
 request_id TEXT, epoch_id TEXT, payload JSONB NOT NULL, canonical BOOLEAN NOT NULL DEFAULT TRUE,
 PRIMARY KEY(chain_id,coordinator,block_hash,tx_hash,log_index)
);
CREATE INDEX IF NOT EXISTS requests_explorer_block ON d20dao_explorer.requests(chain_id,coordinator,request_block);
CREATE INDEX IF NOT EXISTS requests_explorer_observed ON d20dao_explorer.requests(chain_id,coordinator,observed_block);
CREATE INDEX IF NOT EXISTS epochs_explorer_block ON d20dao_explorer.epochs(chain_id,registry,block_number);
CREATE INDEX IF NOT EXISTS events_explorer_block ON d20dao_explorer.events(chain_id,coordinator,block_number);
-- Live keeper status: one row per deployment, replaced about once a minute. Public facts and fault codes only.
CREATE TABLE IF NOT EXISTS d20dao_explorer.keeper_status (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, keeper TEXT NOT NULL,
 healthy BOOLEAN NOT NULL, send_enabled BOOLEAN NOT NULL, faults JSONB NOT NULL,
 observed_at BIGINT NOT NULL, published_at BIGINT NOT NULL, keeper_balance TEXT NOT NULL,
 head_block BIGINT NOT NULL, pending_requests BIGINT NOT NULL, last_served_at BIGINT,
 PRIMARY KEY(chain_id,coordinator)
);
-- Additive: the role of the keeper that wrote the row. A follower replaces the row only when it is its own or stale.
ALTER TABLE d20dao_explorer.keeper_status ADD COLUMN IF NOT EXISTS role TEXT NOT NULL DEFAULT 'primary';
-- Bounded, restartable request refresh. Public rows/cursor change only at final batch commit.
CREATE TABLE IF NOT EXISTS d20dao_explorer.refresh_batches (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, expected_next BIGINT NOT NULL,
 start_block BIGINT NOT NULL, end_block BIGINT NOT NULL, end_hash TEXT NOT NULL,
 reorg BOOLEAN NOT NULL, last_id TEXT NOT NULL DEFAULT '', done BOOLEAN NOT NULL DEFAULT FALSE,
 PRIMARY KEY(chain_id,coordinator)
);
CREATE TABLE IF NOT EXISTS d20dao_explorer.refresh_requests (
 chain_id TEXT NOT NULL, coordinator TEXT NOT NULL, request_id TEXT NOT NULL, record JSONB NOT NULL,
 PRIMARY KEY(chain_id,coordinator,request_id),
 FOREIGN KEY(chain_id,coordinator) REFERENCES d20dao_explorer.refresh_batches(chain_id,coordinator) ON DELETE CASCADE
);
