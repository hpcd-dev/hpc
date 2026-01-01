// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .compile_protos(&["./protos/agent.proto"], &["./protos"])
        .expect("failed to compile protos");
    Ok(())
}
