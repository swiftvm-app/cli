//! Human-readable output. Each stream gets its own terminal settings.

use std::io::{self, IsTerminal};
use std::os::fd::AsRawFd;

use host::{MachineState, NAME};
use textwrap::{Options, WordSplitter, core::display_width};

use crate::plain;

#[derive(Clone, Copy)]
pub enum Tone {
    Heading,
    Bold,
    Muted,
    Good,
    Warning,
    Error,
}

#[derive(Clone, Copy)]
pub struct Ui {
    pub color: bool,
    /// Width of an interactive terminal that can redraw a line.
    pub columns: Option<usize>,
}

impl Ui {
    pub fn new(stream: impl IsTerminal + AsRawFd, allow_color: bool) -> Self {
        let terminal = stream.is_terminal() && std::env::var_os("TERM").as_deref() != Some("dumb".as_ref());
        let columns = terminal.then(|| {
            // Safety: TIOCGWINSZ writes one winsize to the supplied address.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::ioctl(stream.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
            if result == 0 && size.ws_col > 0 {
                usize::from(size.ws_col)
            } else {
                80
            }
        });
        let color =
            allow_color && terminal && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty());
        Self { color, columns }
    }

    pub fn stdout() -> Self {
        Self::new(io::stdout(), true)
    }

    pub fn stderr() -> Self {
        Self::new(io::stderr(), true)
    }

    /// Only the renderer supplies control characters. Names, paths and guest data are single-line text.
    pub fn paint(self, tone: Tone, text: &str) -> String {
        let text = plain(text);
        if !self.color || text.is_empty() {
            return text;
        }
        let code = match tone {
            Tone::Heading => "1;36",
            Tone::Bold => "1",
            Tone::Muted => "2",
            Tone::Good => "32",
            Tone::Warning => "33",
            Tone::Error => "1;31",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }

    pub fn version(self) -> String {
        format!(
            "{} {}",
            self.paint(Tone::Heading, NAME),
            self.paint(Tone::Muted, env!("CARGO_PKG_VERSION"))
        )
    }

    pub fn state(self, state: MachineState) -> String {
        let (tone, label) = match state {
            MachineState::Running => (Tone::Good, "running"),
            MachineState::Stopped => (Tone::Warning, "stopped"),
            MachineState::Broken => (Tone::Error, "broken"),
        };
        self.paint(tone, label)
    }

    fn wrapping<'a>(self) -> Options<'a> {
        Options::new(self.columns.unwrap_or(80).min(88))
            .word_splitter(WordSplitter::NoHyphenation)
            .break_words(false)
    }

    pub fn wrap(self, text: &str, first: &str, rest: &str) -> String {
        textwrap::fill(
            text,
            self.wrapping().initial_indent(first).subsequent_indent(rest),
        )
    }

    /// Wrapped examples can be pasted directly into a shell.
    pub fn command(self, text: &str) -> String {
        let mut options = self.wrapping().initial_indent("  ").subsequent_indent("    ");
        options.width = options.width.saturating_sub(2);
        textwrap::wrap(text, options).join(" \\\n")
    }

    /// Align labels and indent wrapped values under the first word of the value.
    pub fn pairs(self, rows: &[(&str, impl AsRef<str>)]) -> String {
        let width = rows
            .iter()
            .map(|(label, _)| display_width(label))
            .max()
            .unwrap_or(0);
        let indent = " ".repeat(width + 4);
        let stacked = self.columns.is_some_and(|cols| cols < indent.len() + 20);
        rows.iter()
            .map(|(label, value)| {
                let key = self.paint(Tone::Bold, label);
                if stacked {
                    let value = self.wrap(value.as_ref(), "    ", "    ");
                    if key.is_empty() {
                        value
                    } else {
                        format!("  {key}\n{value}")
                    }
                } else {
                    let pad = " ".repeat(width - display_width(label) + 2);
                    self.wrap(value.as_ref(), &format!("  {key}{pad}"), &indent)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A narrow terminal gets one record at a time, with the same headings as the table.
    pub fn table<const N: usize>(self, headers: [&str; N], rows: impl Iterator<Item = [String; N]>) {
        let rows: Vec<_> = rows.collect();
        let widths: [usize; N] = std::array::from_fn(|i| {
            rows.iter()
                .map(|row| display_width(&row[i]))
                .fold(display_width(headers[i]), usize::max)
        });
        let length = widths.iter().sum::<usize>() + 2 * N.saturating_sub(1);
        if !rows.is_empty() && self.columns.is_some_and(|cols| length > cols) {
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                let fields: Vec<_> = headers.into_iter().zip(row).collect();
                println!("{}", self.pairs(&fields));
            }
            return;
        }
        let header = headers.map(|label| self.paint(Tone::Heading, label));
        for row in std::iter::once(header).chain(rows) {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                line.push_str(cell);
                if i + 1 < N {
                    line.push_str(&" ".repeat(widths[i] - display_width(cell) + 2));
                }
            }
            println!("{line}");
        }
    }
}

pub fn success(json: bool, message: String) {
    if !json {
        let ui = Ui::stdout();
        let mark = if ui.color {
            ui.paint(Tone::Good, "✓ ")
        } else {
            String::new()
        };
        println!("{mark}{}", plain(&message));
    }
}

pub fn error(message: &str) {
    let label = Ui::stderr().paint(Tone::Error, &format!("{NAME}:"));
    eprintln!("{label} {}", plain(message));
}
