//! Keys as the guest's screen takes them: X keysyms, which say what a key types rather than where
//! it is. The VNC server finds the key for each, or adds one for a character that its keyboard
//! lacks, and holds Shift where one needs it, or lets go of it where one needs it not.

use proto::Input;

use crate::{Error, ErrorCode, Result};

/// Keys by name, as they are written in a chord: `Return`, `ctrl+s`. Matched without regard to
/// case.
const NAMED: &[(&str, u32)] = &[
    ("return", 0xff0d),
    ("enter", 0xff0d),
    ("escape", 0xff1b),
    ("esc", 0xff1b),
    ("tab", 0xff09),
    ("backspace", 0xff08),
    ("delete", 0xffff),
    ("del", 0xffff),
    ("insert", 0xff63),
    ("space", 0x20),
    ("home", 0xff50),
    ("end", 0xff57),
    ("pageup", 0xff55),
    ("page_up", 0xff55),
    ("pagedown", 0xff56),
    ("page_down", 0xff56),
    ("left", 0xff51),
    ("up", 0xff52),
    ("right", 0xff53),
    ("down", 0xff54),
    ("menu", 0xff67),
    ("print", 0xff61),
    ("printscreen", 0xff61),
    ("capslock", 0xffe5),
    ("plus", 0x2b),
    ("minus", 0x2d),
    // Modifiers, which also work on their own.
    ("ctrl", 0xffe3),
    ("control", 0xffe3),
    ("shift", 0xffe1),
    ("alt", 0xffe9),
    ("option", 0xffe9),
    ("altgr", 0xfe03),
    ("super", 0xffeb),
    ("win", 0xffeb),
    ("meta", 0xffeb),
    ("cmd", 0xffeb),
    ("command", 0xffeb),
];

/// The keysym that types `character`: Latin-1 as itself, the rest of Unicode as X numbers it.
/// None for a control character other than a line break or a tab.
fn keysym(character: char) -> Option<u32> {
    match character {
        '\n' | '\r' => Some(0xff0d),
        '\t' => Some(0xff09),
        _ if character.is_control() => None,
        ' '..='~' | '\u{a0}'..='\u{ff}' => Some(character as u32),
        other => Some(0x0100_0000 | other as u32),
    }
}

/// The keysym of one key of a chord: a name, F1 to F24, or a character.
fn key(name: &str) -> Option<u32> {
    let lower = name.to_lowercase();
    if let Some((_, keysym)) = NAMED.iter().find(|(known, _)| *known == lower) {
        return Some(*keysym);
    }
    if let Some(number) = lower
        .strip_prefix('f')
        .and_then(|number| number.parse::<u32>().ok())
        && (1..=24).contains(&number)
    {
        return Some(0xffbe + number - 1);
    }
    let mut characters = name.chars();
    match (characters.next(), characters.next()) {
        // A letter in a chord is the key, not the capital that Shift would type: ctrl+S is ctrl+s.
        (Some(character), None) if character != ' ' => {
            keysym(character.to_lowercase().next().unwrap_or(character))
        }
        _ => None,
    }
}

const SHIFT: u32 = 0xffe1;
/// How long to wait between the chords of one call, in milliseconds.
const BETWEEN_CHORDS: u32 = 150;

fn press(keysym: u32, events: &mut Vec<Input>) {
    events.push(Input::Key { keysym, down: true });
}

fn release(keysym: u32, events: &mut Vec<Input>) {
    events.push(Input::Key { keysym, down: false });
}

/// The events that press each of `chords` in turn: `Return`, `ctrl+shift+t`, `F5`. The keys of a
/// chord go down in order and come up the other way round.
pub(crate) fn chords(chords: &[String]) -> Result<Vec<Input>> {
    let mut events = Vec::new();
    for (index, chord) in chords.iter().enumerate() {
        // What a chord does may take the application a moment, a paste for one, which a key
        // after it would otherwise overtake.
        if index > 0 {
            events.push(Input::Pause {
                milliseconds: BETWEEN_CHORDS,
            });
        }
        // `+` is a key too: `ctrl++`, or `+` alone.
        let parts: Vec<&str> = if chord == "+" {
            vec!["+"]
        } else if let Some(rest) = chord.strip_suffix("++") {
            rest.split('+').chain(["+"]).collect()
        } else {
            chord.split('+').collect()
        };
        let mut keysyms = parts
            .iter()
            .map(|part| key(part.trim()))
            .collect::<Option<Vec<u32>>>()
            .filter(|keysyms| !keysyms.is_empty())
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "\"{chord}\" isn't a key. Write one like Return, Escape, Tab, F5, a, ctrl+s or ctrl+shift+t."
                    ),
                )
            })?;
        // With Shift held, a letter is its capital: the server would let go of Shift for the
        // small one, and ctrl+shift+v would be ctrl+v.
        if keysyms.contains(&SHIFT) {
            for keysym in &mut keysyms {
                if (b'a' as u32..=b'z' as u32).contains(keysym) {
                    *keysym -= 32;
                }
            }
        }
        for &keysym in &keysyms {
            press(keysym, &mut events);
        }
        for &keysym in keysyms.iter().rev() {
            release(keysym, &mut events);
        }
    }
    Ok(events)
}

/// The events that type `text`, one key after the other. Refused whole if it has a control
/// character other than a line break or a tab.
pub(crate) fn text(text: &str) -> Result<Vec<Input>> {
    let mut events = Vec::new();
    // A line break written as \r\n is one Return.
    for character in text.replace("\r\n", "\n").chars() {
        let keysym = keysym(character).ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidArgument,
                format!(
                    "{character:?} can't be typed: it is a control character. Press keys for it instead."
                ),
            )
        })?;
        press(keysym, &mut events);
        release(keysym, &mut events);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(events: &[Input]) -> Vec<(u32, bool)> {
        events
            .iter()
            .filter_map(|event| match event {
                Input::Key { keysym, down } => Some((*keysym, *down)),
                Input::Pause { .. } => None,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_chord_presses_in_order_and_lets_go_the_other_way() {
        let events = chords(&["ctrl+Shift+T".into(), "Return".into()]).unwrap();
        assert_eq!(
            keys(&events),
            [
                (0xffe3, true),
                (0xffe1, true),
                (b'T' as u32, true),
                (b'T' as u32, false),
                (0xffe1, false),
                (0xffe3, false),
                (0xff0d, true),
                (0xff0d, false),
            ]
        );
        assert!(events.contains(&Input::Pause {
            milliseconds: BETWEEN_CHORDS
        }));
        assert_eq!(keys(&chords(&["F5".into()]).unwrap())[0], (0xffc2, true));
        assert_eq!(keys(&chords(&["ctrl++".into()]).unwrap())[1], (b'+' as u32, true));
        assert_eq!(keys(&chords(&["+".into()]).unwrap())[0], (b'+' as u32, true));
        assert_eq!(keys(&chords(&["shift".into()]).unwrap()).len(), 2);
        // A capital with Shift, so that the server keeps Shift held.
        assert_eq!(
            keys(&chords(&["ctrl+shift+v".into()]).unwrap())[2],
            (b'V' as u32, true)
        );
        assert_eq!(keys(&chords(&["ctrl+V".into()]).unwrap())[1], (b'v' as u32, true));
    }

    #[test]
    fn what_isnt_a_key_is_refused() {
        for wrong in ["", "ctrl+", "hyper+x", "F25", "ab", "ctrl+\u{7}"] {
            assert_eq!(
                chords(&[wrong.into()]).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{wrong}"
            );
        }
    }

    #[test]
    fn text_is_typed_by_what_each_character_is() {
        let events = keys(&text("Aé€\r\n\t").unwrap());
        assert_eq!(
            events,
            [
                (b'A' as u32, true),
                (b'A' as u32, false),
                (0xe9, true),
                (0xe9, false),
                (0x0100_20ac, true),
                (0x0100_20ac, false),
                (0xff0d, true),
                (0xff0d, false),
                (0xff09, true),
                (0xff09, false),
            ]
        );
        for wrong in ["\u{7}", "a\u{1b}[31m"] {
            assert_eq!(
                text(wrong).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{wrong:?}"
            );
        }
    }
}
