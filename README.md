# swiftvm

Creates and runs Linux virtual machines for AI agents, on macOS and Linux, on arm64 and x86-64. No daemon: each running machine is a process of its own.

## Install from source

```sh
scripts/install.sh            # into ~/.local/bin
scripts/install.sh ~/bin      # or into another folder
```

| System | Needs | Gets |
|---|---|---|
| macOS 14 or later | Rust, and the Linux musl target for this processor (`rustup target add aarch64-unknown-linux-musl`), which the guest agent is built for | The program, signed with the virtualization entitlement |
| Linux with KVM | Rust with the musl target for this processor, `musl-tools`, a C compiler and `curl`. The user must be in the `kvm` group | A static program, with `passt` next to it |

Installing again while machines run is safe: they keep the program they started with. To uninstall, delete the program (and `passt` on Linux) from the folder.

## Use

```sh
swiftvm create debian-13 --name dev
swiftvm start dev
swiftvm wait dev
swiftvm ssh dev
swiftvm help
```

Apps use `--json`, described in [docs/api.md](docs/api.md).
