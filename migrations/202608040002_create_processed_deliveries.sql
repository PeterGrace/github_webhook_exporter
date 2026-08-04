CREATE TABLE processed_deliveries (
    delivery_id TEXT PRIMARY KEY,
    received_at TEXT NOT NULL
);

CREATE INDEX processed_deliveries_received_at_idx
    ON processed_deliveries(received_at);
