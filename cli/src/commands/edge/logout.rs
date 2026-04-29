// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use std::fs;

pub async fn run() -> anyhow::Result<()> {
    let dir = dirs_next::home_dir()
        .map(|h| h.join(".aegis").join("edge"))
        .unwrap_or_else(|| std::path::PathBuf::from(".aegis/edge"));
    for f in ["node.token", "node.key", "node.key.pub", "enrollment.jwt"] {
        let p = dir.join(f);
        if p.exists() {
            fs::remove_file(&p).ok();
        }
    }
    println!(
        "aegis edge logout: local credentials removed at {}",
        dir.display()
    );
    Ok(())
}
