//! The guest's desktop, as the library works it: pictures of its screen, its keyboard and
//! pointer, and what is done in the account's session there (opening things, the clipboard).
//!
//! The screen is the VNC server that a desktop setup script runs in the guest. The machine's
//! process holds a connection to it ([`crate::screen`]), and passes others on from its `vnc`
//! socket ([`Host::vnc_socket`]).

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use proto::{Input, ScreenSize, buttons};
use serde::{Deserialize, Serialize};

use crate::{Error, ErrorCode, ExecOptions, Host, Result, client, keys};

/// The most events one request carries, which keeps it well within a line of the protocol.
const EVENTS_AT_ONCE: usize = 200;
/// How long a screen has to stay the same to count as settled.
const SETTLED: Duration = Duration::from_millis(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
}

impl Button {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "left" => Some(Self::Left),
            "middle" => Some(Self::Middle),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    fn bit(self) -> u8 {
        match self {
            Self::Left => buttons::LEFT,
            Self::Middle => buttons::MIDDLE,
            Self::Right => buttons::RIGHT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            _ => None,
        }
    }

    fn bit(self) -> u8 {
        match self {
            Self::Up => buttons::WHEEL_UP,
            Self::Down => buttons::WHEEL_DOWN,
            Self::Left => buttons::WHEEL_LEFT,
            Self::Right => buttons::WHEEL_RIGHT,
        }
    }
}

/// A picture of the guest's screen.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// The picture as a PNG file.
    pub png: Vec<u8>,
}

fn pause(milliseconds: u32) -> Input {
    Input::Pause { milliseconds }
}

fn pointer(x: u32, y: u32, buttons: u8) -> Input {
    Input::Pointer { x, y, buttons }
}

/// What, in a shell, puts a command into the account's desktop session: its display, its message
/// bus where it has one, and the system's language, without which what is opened takes text for
/// ASCII.
const SESSION: &str = r#"set -a
for file in /etc/locale.conf /etc/default/locale; do
    [ ! -r "$file" ] || . "$file"
done
set +a
export LANG="${LANG:-C.UTF-8}"
uid=$(id -u)
export XDG_RUNTIME_DIR=/run/user/$uid DISPLAY=:0 XAUTHORITY=/run/user/$uid/Xauthority
[ ! -S "$XDG_RUNTIME_DIR/bus" ] || export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
[ -S /tmp/.X11-unix/X0 ] || { echo "The guest shows no desktop. A machine gets one from a desktop setup script, such as --script xfce." >&2; exit 3; }
"#;

/// Opens `$1`: a URL, a file, a command, or an application by the name it shows or the name of
/// its file. Left running on its own.
const OPEN: &str = r#"what=$1
detach() { setsid -f "$@" > /dev/null 2>&1 < /dev/null; }
# An application's file whose name or file name is the one asked for, or with --part, has it in it.
application() {
    for file in "$HOME/.local/share/applications"/*.desktop /usr/local/share/applications/*.desktop \
        /usr/share/applications/*.desktop /var/lib/flatpak/exports/share/applications/*.desktop \
        /var/lib/snapd/desktop/applications/*.desktop; do
        [ -f "$file" ] || continue
        name=$(sed -n 's/^Name=//p' "$file" | head -n 1)
        base=$(basename "$file" .desktop)
        if [ "$1" = --part ]; then
            printf '%s\n%s\n' "$name" "$base" | grep -qiF -- "$what" && { echo "$file"; return; }
        else
            printf '%s\n%s\n' "$name" "$base" | grep -qixF -- "$what" && { echo "$file"; return; }
        fi
    done
}
case $what in
    *://*) detach xdg-open "$what"; exit ;;
esac
if [ -e "$what" ]; then
    detach xdg-open "$what"
elif command -v -- "$what" > /dev/null 2>&1; then
    detach "$what"
else
    file=$(application)
    [ -n "$file" ] || file=$(application --part)
    [ -n "$file" ] || { echo "Nothing called \"$what\" is in the guest: no file, command or application." >&2; exit 1; }
    # Without the codes that stand for files to open.
    command=$(sed -n 's/^Exec=//p' "$file" | head -n 1 | sed 's/ *%[fFuUdDnNickvm]//g')
    detach sh -c "$command"
fi
"#;

const CLIPBOARD_GET: &str = r#"command -v xclip > /dev/null || { echo "xclip isn't in the guest. The desktop setup scripts install it." >&2; exit 1; }
xclip -selection clipboard -o 2> /dev/null || true
"#;

/// xclip stays to hand the text to whoever pastes it, so it is left running on its own. It is
/// waited for until the clipboard has the text: a paste right after this gets it.
const CLIPBOARD_SET: &str = r#"command -v xclip > /dev/null || { echo "xclip isn't in the guest. The desktop setup scripts install it." >&2; exit 1; }
text=$(mktemp)
trap 'rm -f "$text"' EXIT
cat > "$text"
setsid -f xclip -selection clipboard -i "$text" > /dev/null 2>&1 < /dev/null
waited=0
until [ "$(xclip -selection clipboard -o 2> /dev/null)" = "$(cat "$text")" ]; do
    [ "$waited" -lt 50 ] || { echo "The clipboard didn't take the text." >&2; exit 1; }
    sleep 0.1
    waited=$((waited + 1))
done
"#;

impl Host {
    /// A picture of the guest's screen as it is now.
    pub fn screenshot(&self, name: &str) -> Result<Picture> {
        let (shot, png) = client::screenshot(&self.store().find(name)?)?;
        Ok(Picture {
            width: shot.width,
            height: shot.height,
            png,
        })
    }

    /// Sends events to the guest's keyboard and pointer, in order, and returns the screen's size.
    pub fn input(&self, name: &str, events: &[Input]) -> Result<ScreenSize> {
        let directory = self.store().find(name)?;
        if events.is_empty() {
            return client::input(&directory, Vec::new());
        }
        let mut size = None;
        for chunk in events.chunks(EVENTS_AT_ONCE) {
            size = Some(client::input(&directory, chunk.to_vec())?);
        }
        Ok(size.expect("there was a chunk"))
    }

    /// The size of the guest's screen, in pixels.
    pub fn screen_size(&self, name: &str) -> Result<ScreenSize> {
        self.input(name, &[])
    }

    /// Clicks `count` times with `button` at a pixel of the screen.
    pub fn click(&self, name: &str, x: u32, y: u32, button: Button, count: u32) -> Result<()> {
        let mut events = vec![pointer(x, y, 0), pause(30)];
        for click in 0..count.clamp(1, 3) {
            if click > 0 {
                events.push(pause(60));
            }
            events.extend([pointer(x, y, button.bit()), pause(20), pointer(x, y, 0)]);
        }
        self.input(name, &events).map(drop)
    }

    /// Puts the pointer at a pixel of the screen.
    pub fn move_pointer(&self, name: &str, x: u32, y: u32) -> Result<()> {
        self.input(name, &[pointer(x, y, 0)]).map(drop)
    }

    /// Holds `button` down at one pixel, moves to another, and lets go there.
    pub fn drag(&self, name: &str, from: (u32, u32), to: (u32, u32), button: Button) -> Result<()> {
        let bit = button.bit();
        let mut events = vec![
            pointer(from.0, from.1, 0),
            pause(50),
            pointer(from.0, from.1, bit),
            pause(150),
        ];
        const STEPS: i64 = 12;
        for step in 1..=STEPS {
            let along =
                |start: u32, end: u32| (start as i64 + (end as i64 - start as i64) * step / STEPS) as u32;
            events.extend([pointer(along(from.0, to.0), along(from.1, to.1), bit), pause(20)]);
        }
        events.extend([pause(150), pointer(to.0, to.1, 0)]);
        self.input(name, &events).map(drop)
    }

    /// Turns the wheel `notches` times, over a pixel of the screen or over its middle.
    pub fn scroll(
        &self,
        name: &str,
        at: Option<(u32, u32)>,
        direction: Direction,
        notches: u32,
    ) -> Result<()> {
        let (x, y) = match at {
            Some(at) => at,
            None => {
                let size = self.screen_size(name)?;
                (size.width / 2, size.height / 2)
            }
        };
        let mut events = vec![pointer(x, y, 0), pause(30)];
        for _ in 0..notches.clamp(1, 100) {
            events.extend([pointer(x, y, direction.bit()), pointer(x, y, 0), pause(15)]);
        }
        self.input(name, &events).map(drop)
    }

    /// Types `text` at the keyboard's focus, a key at a time.
    pub fn type_text(&self, name: &str, text: &str) -> Result<()> {
        let events = keys::text(text)?;
        self.input(name, &events).map(drop)
    }

    /// Presses keys and chords one after the other: `Return`, `ctrl+shift+t`, `F5`.
    pub fn press_keys(&self, name: &str, chords: &[String]) -> Result<()> {
        let events = keys::chords(chords)?;
        self.input(name, &events).map(drop)
    }

    /// Waits until the screen has stopped changing, for `timeout` at most. Says whether it did.
    pub fn wait_for_screen(&self, name: &str, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut last = self.screenshot(name)?.png;
        let mut since = Instant::now();
        loop {
            if since.elapsed() >= SETTLED {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(200));
            let now = self.screenshot(name)?.png;
            if now != last {
                last = now;
                since = Instant::now();
            }
        }
    }

    /// Where the machine's process passes VNC connections on to the guest's screen: a Unix
    /// socket, to which a viewer or an app connects as to any VNC server. There is no password:
    /// only this user can open it. Connections share the screen.
    pub fn vnc_socket(&self, name: &str) -> Result<PathBuf> {
        client::vnc_socket(&self.store().find(name)?)
    }

    /// Runs a shell script in the account's desktop session, and returns what it printed.
    fn in_session(
        &self,
        name: &str,
        script: &str,
        arguments: &[&str],
        input: Option<String>,
    ) -> Result<String> {
        let mut words = vec![
            "sh".to_string(),
            "-c".into(),
            format!("{SESSION}{script}"),
            "sh".into(),
        ];
        words.extend(arguments.iter().map(|argument| argument.to_string()));
        let options = ExecOptions {
            arguments: words,
            timeout: Some(Duration::from_secs(30)),
            ..ExecOptions::default()
        };
        let input =
            input.map(|text| Box::new(std::io::Cursor::new(text.into_bytes())) as Box<dyn Read + Send>);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let ended = self.exec(name, options, input, &mut output, &mut errors)?;
        if let Some(error) = ended.error {
            return Err(error);
        }
        if !ended.success() {
            let said = String::from_utf8_lossy(&errors).trim().to_string();
            return Err(Error::new(
                ErrorCode::Guest,
                if said.is_empty() {
                    "The guest couldn't do it, and said nothing about why.".to_string()
                } else {
                    said
                },
            ));
        }
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    /// Opens an application by its name or its command, a file, or a URL in the guest's desktop.
    pub fn open_in_guest(&self, name: &str, what: &str) -> Result<()> {
        if what.trim().is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "Say what to open."));
        }
        self.in_session(name, OPEN, &[what], None).map(drop)
    }

    /// The text on the guest's clipboard.
    pub fn clipboard(&self, name: &str) -> Result<String> {
        self.in_session(name, CLIPBOARD_GET, &[], None)
    }

    /// Puts text on the guest's clipboard.
    pub fn set_clipboard(&self, name: &str, text: &str) -> Result<()> {
        self.in_session(name, CLIPBOARD_SET, &[], Some(text.to_string()))
            .map(drop)
    }
}
