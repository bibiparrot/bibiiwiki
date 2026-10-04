# Upstream provenance

This directory contains the unmodified Rust source and tests from
`litellm-rust/crates/core` in the LiteLLM repository:

- Repository: <https://github.com/BerriAI/litellm>
- Revision: `bb72815e7062451adf1547df03080f39eb908cc1`
- Upstream path: `litellm-rust/crates/core`
- License: MIT; see `LICENSE` in this directory

Only `Cargo.toml` was made standalone by replacing inherited workspace package
fields and dependencies with the values from upstream's
`litellm-rust/Cargo.toml`. The crate source is otherwise unchanged.
