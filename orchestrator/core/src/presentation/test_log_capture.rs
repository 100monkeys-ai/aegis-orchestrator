// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Test support: capture every tracing event and span field, as text, while a
//! future runs on the current thread. Core has no `tracing-subscriber`; this
//! is the smallest subscriber that records what a log line would carry.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

struct Capture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={:?} ", field.name(), value);
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        let mut out = self.0.lock().unwrap();
        span.record(&mut Fields(&mut out));
        out.push('\n');
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        let mut out = self.0.lock().unwrap();
        values.record(&mut Fields(&mut out));
        out.push('\n');
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut out = self.0.lock().unwrap();
        event.record(&mut Fields(&mut out));
        out.push('\n');
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// Run `future` to completion on a current-thread runtime with every event
/// and span field captured, and return what was captured with the output.
pub(crate) fn capture_logs<F: std::future::Future>(future: F) -> (F::Output, String) {
    let captured = Arc::new(Mutex::new(String::new()));
    let _guard = tracing::subscriber::set_default(Capture(captured.clone()));
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future);
    let text = captured.lock().unwrap().clone();
    (output, text)
}
