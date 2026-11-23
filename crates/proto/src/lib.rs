//! What the host's library, the command and the guest's agent agree on: the product's name, errors
//! with codes that stay the same, and the messages of a machine's control socket.

mod control;
mod error;

pub use control::{
    Hello, LONGEST_LINE, OLDEST_PROTOCOL, PROTOCOL, Request, Response, State, Status, agreed, greet, receive,
    send, welcome,
};
pub use error::{Error, ErrorCode, Result};

/// The product's name: the one line in the `NAME` file at the repository's root. Nothing else
/// spells it out, so changing that file renames the product.
pub const NAME: &str = include_str!("../../../NAME").trim_ascii();

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
