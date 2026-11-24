//! What a cloud image sets itself up from when it first starts: a small disc called `cidata`,
//! which cloud-init looks for.

use std::collections::BTreeMap;
use std::path::Path;

use proto::NAME;

use crate::scripts::{Script, guest_text, line};
use crate::{Error, ErrorCode, Result, iso};

pub(crate) struct Seed<'a> {
    pub host_name: &'a str,
    /// cloud-init sets a system up once for each of these that it sees.
    pub instance_id: &'a str,
    pub user: &'a str,
    /// Without one, nobody can sign in to the account with a password.
    pub password: Option<&'a str>,
    /// The network card to configure.
    pub mac_address: &'a str,
    /// The guest's agent, which the first start installs.
    pub agent: &'a [u8],
    /// What the first start sets up once it is over, one after the other.
    pub scripts: &'a [Script],
}

/// What installs the agent and the setup scripts: the files on the disc, with the folder the disc
/// is mounted at.
const INSTALL: &str = include_str!("../guest/install.sh");
const AGENT_UNIT: &str = include_str!("../guest/agent.service");
const SETUP_RUNNER: &str = include_str!("../guest/setup-run.sh");
const SETUP_UNIT: &str = include_str!("../guest/setup.service");

/// Mounts the disc and runs its `install.sh`. In a shell of its own, which cloud-init's script of
/// commands goes on after.
const RUN_INSTALL: &str = "(seed=$(mktemp -d) && device=$(blkid -L cidata || blkid -L CIDATA) \
&& mount -o ro \"$device\" \"$seed\" && { sh \"$seed/install.sh\" \"$seed\"; status=$?; umount \"$seed\"; rmdir \"$seed\"; exit $status; })";

/// What is wrong with `user` as the name of the account to create.
pub(crate) fn check_user(user: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(Error::new(
            ErrorCode::InvalidArgument,
            format!("\"{user}\" can't be the account's name. {why}"),
        ))
    };
    let allowed =
        |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_';
    if !(1..=32).contains(&user.len())
        || !user.as_bytes()[0].is_ascii_lowercase()
        || !user.bytes().all(allowed)
    {
        return invalid("Use up to 32 lowercase letters, digits, \"-\" and \"_\", with a letter first.");
    }
    if user == "root" {
        return invalid("Choose another name than root.");
    }
    Ok(())
}

/// A name for the account that is as close to `name` as the rules allow: `Jane.Doe`, as a
/// computer may call its user, becomes `janedoe`.
pub(crate) fn user_from(name: &str) -> String {
    let kept: String = name
        .to_ascii_lowercase()
        .chars()
        .filter(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || "-_".contains(*character)
        })
        .skip_while(|character| !character.is_ascii_lowercase())
        .take(32)
        .collect();
    if check_user(&kept).is_ok() {
        kept
    } else {
        "user".into()
    }
}

/// A string as YAML reads it: a JSON string, with a few more characters written as escapes.
/// YAML refuses a document that holds them as they are, or reads a line break in their place.
fn yaml(value: &str) -> String {
    let json = serde_json::to_string(value).expect("a string is JSON");
    let mut quoted = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '\u{7F}'..='\u{9F}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}' => {
                quoted.push_str(&format!("\\u{:04X}", character as u32));
            }
            _ => quoted.push(character),
        }
    }
    quoted
}

impl Seed<'_> {
    fn meta_data(&self) -> String {
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            yaml(self.instance_id),
            yaml(self.host_name)
        )
    }

    fn user_data(&self) -> String {
        let user = yaml(self.user);
        let mut lines = vec![
            "#cloud-config".to_string(),
            format!("hostname: {}", yaml(self.host_name)),
            "manage_etc_hosts: true".into(),
            "users:".into(),
            format!("  - name: {user}"),
            "    shell: /bin/bash".into(),
            format!("    sudo: {}", yaml("ALL=(ALL) NOPASSWD:ALL")),
            format!("    lock_passwd: {}", self.password.is_none()),
        ];
        match self.password {
            Some(password) => lines.extend([
                "ssh_pwauth: true".into(),
                "chpasswd:".into(),
                "  expire: false".into(),
                "  users:".into(),
                format!("    - name: {user}"),
                format!("      password: {}", yaml(password)),
                "      type: text".into(),
            ]),
            None => lines.push("ssh_pwauth: false".into()),
        }
        lines.extend([
            "growpart:".into(),
            "  mode: auto".into(),
            "  devices: [\"/\"]".into(),
            "write_files:".into(),
            format!("  - path: {}", yaml(&console_settings_path())),
            format!("    content: {}", yaml(CONSOLE_SETTINGS)),
            "runcmd:".into(),
            format!("  - {}", yaml(UPDATE_BOOT_MENU)),
            format!("  - {}", yaml(RUN_INSTALL)),
        ]);
        lines.join("\n") + "\n"
    }

    /// The card is found by its address and names itself to DHCP by it. cloud-init's own choice,
    /// a made-up identifier, would get the machine another address after every new setup.
    fn network_config(&self) -> String {
        format!(
            "version: 2\nethernets:\n  primary:\n    match:\n      macaddress: {}\n    dhcp4: true\n    dhcp6: true\n    dhcp-identifier: mac\n",
            yaml(&self.mac_address.to_ascii_lowercase())
        )
    }

    /// What the setup runner is told about the scripts and the machine, one `key=value` to a
    /// line. It reads the lines and never runs them, so a value can be anything on one line.
    fn settings(&self) -> String {
        let names: Vec<&str> = self.scripts.iter().map(|script| script.name.as_str()).collect();
        format!(
            "name={}\nrestart={}\nuser={}\n",
            line(&names.join(", "), 200),
            if self.scripts.iter().any(|script| script.restart) {
                "yes"
            } else {
                "no"
            },
            self.user
        )
    }

    /// The disc's files, by name.
    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = vec![
            ("meta-data".into(), self.meta_data().into_bytes()),
            ("user-data".into(), self.user_data().into_bytes()),
            ("network-config".into(), self.network_config().into_bytes()),
            ("agent".into(), self.agent.to_vec()),
            ("agent.service".into(), guest_text(AGENT_UNIT).into_bytes()),
            ("install.sh".into(), guest_text(INSTALL).into_bytes()),
        ];
        if !self.scripts.is_empty() {
            files.push(("setup-run.sh".into(), guest_text(SETUP_RUNNER).into_bytes()));
            files.push(("setup.service".into(), guest_text(SETUP_UNIT).into_bytes()));
            files.push(("settings".into(), self.settings().into_bytes()));
        }
        for (index, script) in self.scripts.iter().enumerate() {
            let number = index + 1;
            files.push((format!("script-{number}"), script.source.clone()));
            files.push((
                format!("script-{number}.name"),
                format!("{}\n", script.name).into_bytes(),
            ));
        }
        files.into_iter().collect()
    }

    /// Writes the disc at `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        iso::write(path, "cidata", &self.files())
    }
}

/// Where a guest that boots with GRUB picks up additions to the kernel's command line. The name
/// sorts after the settings that cloud images bring, so that what it adds comes last.
fn console_settings_path() -> String {
    format!("/etc/default/grub.d/99-{NAME}-console.cfg")
}

/// Cloud images send the kernel's and the system's messages to a serial port that these machines
/// don't have. This adds the console they do have, `hvc0`, and names it last, which makes it the
/// one that the system reports to and that an emergency shell comes up on. It takes effect from
/// the second start: the first one has read its command line already.
const CONSOLE_SETTINGS: &str = "GRUB_CMDLINE_LINUX_DEFAULT=\"$GRUB_CMDLINE_LINUX_DEFAULT console=hvc0\"\n";

/// Writes the boot menu anew with the settings above, on the systems that keep them there.
const UPDATE_BOOT_MENU: &str = "if command -v update-grub >/dev/null; then update-grub; fi; true";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_that_yaml_refuses_are_written_as_escapes() {
        assert_eq!(
            yaml("a\u{7f}b\u{85}c\u{2028}\"d"),
            r#""a\u007Fb\u0085c\u2028\"d""#
        );
    }

    #[test]
    fn a_password_goes_into_the_settings_as_it_is() {
        let seed = Seed {
            host_name: "a",
            instance_id: "1234",
            user: "username",
            password: Some("it's \"#: {odd}\\"),
            mac_address: "52:54:00:AA:BB:CC",
            agent: b"agent",
            scripts: &[],
        };
        let settings = seed.user_data();
        assert!(settings.starts_with("#cloud-config\n"));
        assert!(settings.contains(r##"      password: "it's \"#: {odd}\\""##));
        assert!(settings.contains("    lock_passwd: false\n"));
        assert!(
            seed.network_config()
                .contains("macaddress: \"52:54:00:aa:bb:cc\"")
        );
    }

    #[test]
    fn the_setup_goes_on_the_disc_only_with_scripts() {
        let mut seed = Seed {
            host_name: "a",
            instance_id: "1234",
            user: "username",
            password: None,
            mac_address: "52:54:00:aa:bb:cc",
            agent: b"agent",
            scripts: &[],
        };
        let files = seed.files();
        assert!(files.contains_key("install.sh") && files.contains_key("agent.service"));
        assert!(!files.contains_key("setup-run.sh"));
        let install = String::from_utf8(files["install.sh"].clone()).unwrap();
        assert!(install.contains(&format!("/usr/local/lib/{NAME}")) && !install.contains('@'));

        let scripts = [Script::find("docker").unwrap(), Script::find("updates").unwrap()];
        seed.scripts = &scripts;
        let files = seed.files();
        assert_eq!(
            String::from_utf8(files["settings"].clone()).unwrap(),
            "name=Docker, Updates\nrestart=yes\nuser=username\n"
        );
        assert!(files.contains_key("script-2") && files.contains_key("setup.service"));
        assert!(seed.user_data().contains("install.sh"));
    }

    /// The system's own reader of discs, where it has one, finds the files under their names.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_disc_is_read_back() {
        let directory = std::env::temp_dir().join(format!("seed-test-{}", crate::random_hex(4).unwrap()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("seed.iso");
        let seed = Seed {
            host_name: "a",
            instance_id: "1234",
            user: "username",
            password: None,
            mac_address: "52:54:00:aa:bb:cc",
            agent: b"agent",
            scripts: &[Script::find("docker").unwrap()],
        };
        seed.write(&path).unwrap();
        for (name, contents) in [
            ("user-data", seed.user_data()),
            ("meta-data", seed.meta_data()),
            ("agent", "agent".into()),
            ("script-1.name", "Docker\n".into()),
        ] {
            let read = std::process::Command::new("/usr/bin/tar")
                .arg("-xOf")
                .arg(&path)
                .arg(name)
                .output()
                .unwrap();
            assert!(read.status.success(), "{}", String::from_utf8_lossy(&read.stderr));
            assert_eq!(String::from_utf8_lossy(&read.stdout), contents);
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
