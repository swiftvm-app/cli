//! What the host's library, the command and the guest's agent agree on: the product's name, errors
//! with codes that stay the same, the messages of a machine's control socket, and those of the
//! guest's agent.

pub mod agent;
mod control;
pub mod copy;
mod error;
pub mod relay;

pub use control::{
    Hello, Input, LONGEST_LINE, OLDEST_PROTOCOL, PROTOCOL, Request, Response, ScreenSize, Screenshot, State,
    Status, agreed, buttons, greet, receive, send, welcome,
};

/// The TCP port in the guest that its desktop's VNC server listens on, on `localhost` only.
pub const SCREEN_PORT: u16 = 5900;
pub use error::{Error, ErrorCode, Result};

/// The product's name: `name` under `[workspace.metadata]` in the workspace's `Cargo.toml`, which
/// the build script passes on.
pub const NAME: &str = env!("PRODUCT_NAME");

/// The name of one of the product's environment variables: `HOME` gives `<NAME>_HOME`.
pub fn variable(suffix: &str) -> String {
    let prefix: String = NAME
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' => character.to_ascii_uppercase(),
            _ => '_',
        })
        .collect();
    format!("{prefix}_{suffix}")
}
