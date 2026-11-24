//! What the host's library, the command and the guest's agent agree on: the product's name, errors
//! with codes that stay the same, and the messages of a machine's control socket.

mod control;
mod error;

pub use control::{
    Hello, LONGEST_LINE, OLDEST_PROTOCOL, PROTOCOL, Request, Response, State, Status, agreed, greet, receive,
    send, welcome,
};
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
