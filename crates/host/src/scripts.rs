//! Setup scripts: what a new machine runs once, after its first start, to set something up.
//!
//! A script says what it is in comments at its top: `# name: Docker`, and `# restart: yes` for
//! one after which the system has to start again.

use std::path::Path;

use proto::{NAME, SCREEN_PORT, variable};

use crate::{Context, Error, ErrorCode, Result};

/// The scripts that come with this program, by the names they are picked with.
const BUILT_IN: &[(&str, &str)] = &[
    ("xfce", include_str!("../guest/scripts/xfce.sh")),
    ("gnome", include_str!("../guest/scripts/gnome.sh")),
    ("kde-plasma", include_str!("../guest/scripts/kde-plasma.sh")),
    ("cinnamon", include_str!("../guest/scripts/cinnamon.sh")),
    ("docker", include_str!("../guest/scripts/docker.sh")),
    ("web-server", include_str!("../guest/scripts/web-server.sh")),
    ("updates", include_str!("../guest/scripts/updates.sh")),
];

/// What the desktop scripts share, put into each where it says `@DESKTOP@`.
const DESKTOP: &str = include_str!("../guest/scripts/desktop.sh");

/// The longest a script's name is kept at.
const LONGEST_NAME: usize = 120;

#[derive(Clone)]
pub(crate) struct Script {
    /// What it sets up, as it calls itself.
    pub name: String,
    pub source: Vec<u8>,
    /// The system has to start again once it has run.
    pub restart: bool,
}

/// A file that goes into a guest, with this program's name in it where its text says `@NAME@`,
/// the name of the variable that holds the machine's account where it says `@USER_VARIABLE@`,
/// the port of the desktop's screen where it says `@SCREEN_PORT@`, and what the desktop scripts
/// share where it says `@DESKTOP@`.
pub(crate) fn guest_text(template: &str) -> String {
    template
        .replace("@DESKTOP@", DESKTOP)
        .replace("@NAME@", NAME)
        .replace("@USER_VARIABLE@", &variable("USER"))
        .replace("@SCREEN_PORT@", &SCREEN_PORT.to_string())
}

/// `text` as one line of no more than `limit` characters, without control characters.
pub(crate) fn line(text: &str, limit: usize) -> String {
    let kept: String = text
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect();
    kept.trim().to_string()
}

/// The names of the scripts that come with this program.
pub fn built_in() -> Vec<&'static str> {
    BUILT_IN.iter().map(|(name, _)| *name).collect()
}

/// What a script's comments at its top say about `key`.
fn header(source: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(&source[..source.len().min(4096)]);
    text.lines()
        .take_while(|line| line.starts_with('#') || line.trim().is_empty())
        .find_map(|line| {
            let (found, value) = line.trim_start_matches('#').split_once(':')?;
            (found.trim() == key).then(|| value.trim().to_string())
        })
}

impl Script {
    fn new(source: Vec<u8>, fallback_name: &str) -> Self {
        let name = header(&source, "name")
            .map(|name| line(&name, LONGEST_NAME))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| line(fallback_name, LONGEST_NAME));
        let restart = header(&source, "restart").is_some_and(|value| value == "yes");
        Self {
            name,
            source,
            restart,
        }
    }

    /// The script that `chosen` names: one that comes with this program, or a file.
    pub fn find(chosen: &str) -> Result<Self> {
        if let Some((name, source)) = BUILT_IN.iter().find(|(name, _)| *name == chosen) {
            return Ok(Self::new(guest_text(source).into_bytes(), name));
        }
        let path = Path::new(chosen);
        if path.is_file() {
            let source = std::fs::read(path).at("read", path)?;
            let stem = path
                .file_stem()
                .map_or(chosen.into(), |stem| stem.to_string_lossy());
            return Ok(Self::new(source, &stem));
        }
        Err(Error::new(
            ErrorCode::InvalidArgument,
            format!(
                "There is no setup script \"{chosen}\". Those that come with {NAME} are {}, and the path of a file works too.",
                built_in().join(", ")
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_says_what_it_is() {
        let docker = Script::find("docker").unwrap();
        assert_eq!(docker.name, "Docker");
        assert!(!docker.restart);
        let source = String::from_utf8(docker.source).unwrap();
        assert!(source.contains(&format!("\"${{{}}}\"", variable("USER"))));
        assert!(!source.contains('@'));
        assert!(Script::find("updates").unwrap().restart);
        for name in built_in() {
            let source = String::from_utf8(Script::find(name).unwrap().source).unwrap();
            // Fedora's groups of packages start with @ too.
            for placeholder in ["@NAME@", "@USER_VARIABLE@", "@SCREEN_PORT@", "@DESKTOP@"] {
                assert!(!source.contains(placeholder), "{name} has {placeholder} left");
            }
        }
        let xfce = String::from_utf8(Script::find("xfce").unwrap().source).unwrap();
        assert!(xfce.contains("desktop_finish xubuntu xfce"));
        assert!(xfce.contains(&format!("-rfbport {SCREEN_PORT} -localhost")));
        assert!(xfce.contains(&format!("{NAME}-desktop.service")));
        assert_eq!(
            Script::find("no-such-script").err().unwrap().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn a_script_without_a_name_is_called_after_its_file() {
        let script = Script::new(b"#!/bin/sh\necho hi\n".to_vec(), "mine");
        assert_eq!(script.name, "mine");
        let named = Script::new(
            b"#!/bin/sh\n# name:  Tools \x07\n# restart: yes\nset -e\n".to_vec(),
            "x",
        );
        assert_eq!(named.name, "Tools");
        assert!(named.restart);
    }
}
