// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The commit history that the code tools read.

use std::sync::LazyLock;

/// Overrides the schema of the commit history the code tools read, such as
/// a build's own `pghacker_history` before it is installed.
pub const HISTORY_ENV: &str = "PGHACKER_CODE_HISTORY";

/// Where an install puts the commit history.
pub const DEFAULT_HISTORY_SCHEMA: &str = "pghacker";

/// The schema of the commit history that the code tools read and anchors
/// resolve to: [`HISTORY_ENV`]'s, else [`DEFAULT_HISTORY_SCHEMA`].
pub fn history_schema() -> &'static str {
    static SCHEMA: LazyLock<String> = LazyLock::new(|| {
        std::env::var(HISTORY_ENV)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_HISTORY_SCHEMA.to_owned())
    });
    &SCHEMA
}
