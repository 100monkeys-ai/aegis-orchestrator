// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression test: the daemon's PostgreSQL start-up does not write the
//! database URL's password to its log.
//!
//! Drives `connect_postgres` — the code `start_daemon` runs — with a
//! configuration whose database URL carries a marker password and a host that
//! cannot resolve, so the connection fails at once and a node that is not
//! labelled production falls back to in-memory repositories. Every event the
//! call emits, at every level, is captured by a formatting subscriber of the
//! same kind the daemon installs, and the captured text is searched for the
//! marker.

use std::io::Write;
use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::domain::node_config::{DatabaseConfig, NodeConfigManifest};

/// Marker password. Its appearance anywhere in the captured output is the leak.
const MARKER: &str = "Mk7-db-password-marker";

/// Host under the reserved `.invalid` top-level domain: resolution fails
/// immediately, which sqlx reports without retrying.
const UNRESOLVABLE_HOST: &str = "aegis-redaction-test.invalid";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test]
async fn postgres_start_up_log_does_not_carry_the_database_password() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut config = NodeConfigManifest::default();
    assert!(
        !config.is_production(),
        "precondition: the default configuration is not labelled production"
    );
    let database: DatabaseConfig = serde_json::from_value(serde_json::json!({
        "url": format!("postgres://aegis:{MARKER}@{UNRESOLVABLE_HOST}:5432/aegis"),
    }))
    .expect("a database section with only a url deserialises");
    config.spec.database = Some(database);

    let pool = super::server::connect_postgres(&config)
        .await
        .expect("a node that is not labelled production falls back instead of failing");
    assert!(pool.is_none(), "an unresolvable host cannot yield a pool");

    let output = captured.text();
    assert!(
        output.contains("Initializing repositories with PostgreSQL"),
        "precondition: the start-up line was not emitted, so nothing was tested:\n{output}"
    );
    assert!(
        output.contains(UNRESOLVABLE_HOST),
        "the start-up line no longer names the database host:\n{output}"
    );
    assert!(
        !output.contains(MARKER),
        "the database URL's password reached the daemon log:\n{output}"
    );
}

/// The driver is handed the database URL exactly as configured: the host,
/// port, user and database, and the password byte for byte. No database is
/// needed; the options are what `connect_with` receives.
#[test]
fn postgres_driver_is_handed_the_url_as_configured() {
    let url = aegis_orchestrator_core::domain::secrets::SensitiveUrl::new(
        "postgres://aegis:Mk7-pg-driver-password-marker@db.internal:5433/aegis_db",
    );
    let options = super::server::pg_connect_options(&url).expect("the URL parses");
    assert_eq!(options.get_host(), "db.internal");
    assert_eq!(options.get_port(), 5433);
    assert_eq!(options.get_username(), "aegis");
    assert_eq!(options.get_database(), Some("aegis_db"));
    // The options expose no password getter; their Debug shows the field.
    let printed = format!("{options:?}");
    assert!(
        printed.contains("password: Some(\"Mk7-pg-driver-password-marker\")"),
        "the driver would connect with a different password than the one configured: {printed}"
    );
}
