//! `watch`: the machines, again each time one of them changes, until stopped.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use host::{Host, Machine, Result};
use serde_json::{Value, json};

use crate::ui::{Tone, Ui};
use crate::{Arguments, Done};

/// How often the store is looked at.
const INTERVAL: Duration = Duration::from_secs(1);

/// What makes a change worth a line: all of a machine but the room its disk takes, which
/// changes whenever a running guest writes.
fn shape(machine: &Machine) -> Value {
    let mut value = json!(machine);
    if let Some(fields) = value.as_object_mut() {
        fields.remove("disk_used");
        fields.remove("disk_shared");
    }
    value
}

/// The lines that tell a person what changed between `before` and `now`.
fn changes(
    ui: Ui,
    before: Option<&BTreeMap<String, Machine>>,
    now: &BTreeMap<String, Machine>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(before) = before else {
        if now.is_empty() {
            lines.push("No machines.".to_string());
        }
        for machine in now.values() {
            lines.push(format!(
                "{}: {}",
                ui.paint(Tone::Bold, &machine.name),
                ui.state(machine.state)
            ));
        }
        return lines;
    };
    for name in before.keys().filter(|name| !now.contains_key(*name)) {
        lines.push(format!(
            "{}: {}",
            ui.paint(Tone::Bold, name),
            ui.paint(Tone::Warning, "deleted")
        ));
    }
    for (name, machine) in now {
        let label = ui.paint(Tone::Bold, name);
        match before.get(name) {
            None => lines.push(format!("{label}: created, {}", ui.state(machine.state))),
            Some(old) if old.state != machine.state => {
                lines.push(format!("{label}: {}", ui.state(machine.state)));
            }
            Some(old) if shape(old) != shape(machine) => lines.push(format!("{label}: changed")),
            Some(_) => {}
        }
    }
    lines
}

/// Whether whoever read standard output has closed their end of it: a pipe or a socket. `watch`
/// writes only when something changes, and would otherwise go on long after its reader.
#[cfg(target_os = "macos")]
fn reader_gone() -> bool {
    // poll says nothing about such a pipe on macOS. A kqueue says it has ended.
    // Safety: the event is set up here and the queue is closed again; a zero timeout doesn't wait.
    unsafe {
        let queue = libc::kqueue();
        if queue < 0 {
            return false;
        }
        let mut change: libc::kevent = std::mem::zeroed();
        change.ident = libc::STDOUT_FILENO as usize;
        change.filter = libc::EVFILT_WRITE;
        change.flags = libc::EV_ADD;
        let mut event: libc::kevent = std::mem::zeroed();
        let now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let found = libc::kevent(queue, &change, 1, &mut event, 1, &now);
        libc::close(queue);
        found == 1 && event.flags & libc::EV_EOF != 0 && event.flags & libc::EV_ERROR == 0
    }
}

/// Whether whoever read standard output has closed their end of it: a pipe or a socket. `watch`
/// writes only when something changes, and would otherwise go on long after its reader.
#[cfg(not(target_os = "macos"))]
fn reader_gone() -> bool {
    let mut descriptor = libc::pollfd {
        fd: libc::STDOUT_FILENO,
        events: 0,
        revents: 0,
    };
    // Safety: one descriptor, and a zero timeout doesn't wait.
    let found = unsafe { libc::poll(&mut descriptor, 1, 0) };
    found == 1 && descriptor.revents & (libc::POLLERR | libc::POLLHUP) != 0
}

pub fn run(words: &[String], json: bool) -> Result<Done> {
    Arguments::parse(words, &[], &[])?.nothing_else()?;
    let host = Host::open()?;
    let mut last: Option<BTreeMap<String, Machine>> = None;
    let mut last_shapes = None;
    let ui = Ui::new(std::io::stdout(), !json);
    loop {
        let machines = host.list()?;
        let shapes: Vec<Value> = machines.iter().map(shape).collect();
        if last_shapes.as_ref() != Some(&shapes) {
            let now: BTreeMap<String, Machine> = machines
                .iter()
                .map(|machine| (machine.name.clone(), machine.clone()))
                .collect();
            let mut output = std::io::stdout().lock();
            let written = if json {
                writeln!(output, "{}", json!({ "machines": machines }))
            } else {
                changes(ui, last.as_ref(), &now)
                    .iter()
                    .try_for_each(|line| writeln!(output, "{line}"))
            };
            // Nobody reads any more.
            if written.and_then(|()| output.flush()).is_err() {
                return Ok(Value::Null.into());
            }
            last = Some(now);
            last_shapes = Some(shapes);
        }
        std::thread::sleep(INTERVAL);
        if reader_gone() {
            return Ok(Value::Null.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use host::MachineState;

    fn machine(name: &str, state: MachineState, disk_used: u64) -> Machine {
        Machine {
            name: name.into(),
            state,
            pid: None,
            problem: None,
            id: "0".into(),
            image: "debian-13".into(),
            arch: "arm64".into(),
            cpus: 2,
            memory: 1 << 30,
            disk_size: 1 << 34,
            disk_used,
            disk_shared: 0,
            user: "username".into(),
            created: 0,
            nested: false,
            shares: Vec::new(),
        }
    }

    fn map(machines: &[Machine]) -> BTreeMap<String, Machine> {
        machines
            .iter()
            .map(|machine| (machine.name.clone(), machine.clone()))
            .collect()
    }

    #[test]
    fn a_person_is_told_what_changed() {
        let before = map(&[
            machine("a", MachineState::Stopped, 1),
            machine("b", MachineState::Running, 1),
        ]);
        let now = map(&[
            machine("a", MachineState::Running, 1),
            machine("c", MachineState::Stopped, 1),
        ]);
        let ui = Ui::new(std::io::stdout(), false);
        assert_eq!(changes(ui, None, &before), ["a: stopped", "b: running"]);
        assert_eq!(changes(ui, None, &BTreeMap::new()), ["No machines."]);
        assert_eq!(
            changes(ui, Some(&before), &now),
            ["b: deleted", "a: running", "c: created, stopped"]
        );
    }

    #[test]
    fn what_the_disk_takes_up_is_no_change() {
        let before = map(&[machine("a", MachineState::Running, 1)]);
        let now = map(&[machine("a", MachineState::Running, 2)]);
        assert!(changes(Ui::new(std::io::stdout(), false), Some(&before), &now).is_empty());
        assert_eq!(shape(&before["a"]), shape(&now["a"]));
    }
}
