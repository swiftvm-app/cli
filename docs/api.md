# The JSON API

Apps drive `swiftvm` by running it with `--json` and reading what it prints. This page is version 1 of that API. `swiftvm version --json` says which version a program speaks.

## Running a command

```sh
swiftvm <command> [arguments] --json
```

| Channel | What it carries |
|---|---|
| Standard output | One JSON document on one line: the result, or an error. `watch` and `forward` are the exceptions below |
| Standard error | Progress, one JSON object per line, from the commands that report it. Nothing else, except from `console` and `ssh`, which connect a terminal |
| Exit status | 0: done. 1: failed. 2: the command line was wrong. `disk check` ends with 1 when a layer is damaged. `exec` ends as a shell would: the command's status, 128 plus the signal that ended it, 124 when it ran out of time, and 1 when it couldn't be run |

`--json` can go anywhere before `--`. What follows `--` belongs to the guest command.

### Errors

```json
{"error": {"code": "not-found", "message": "There is no machine called \"a\"."}}
```

`message` is for people. Programs act on `code`:

| Code | Meaning |
|---|---|
| `invalid-argument` | A name, size or option that can't be used. Exit status 2 |
| `not-found` | No machine, image or snapshot goes by that name |
| `already-exists` | The name is taken |
| `running` | The machine has to be stopped for this |
| `not-running` | The machine has to be running for this |
| `download` | An image or the engine couldn't be downloaded |
| `checksum` | A download isn't what its publisher says it should be |
| `image` | An image is damaged, or in a format that isn't read |
| `unsupported` | This computer or this build can't do it |
| `engine` | The virtualization engine refused or failed |
| `protocol` | The other side of a connection said something that wasn't understood |
| `io` | A file or a socket failed |
| `guest` | The guest's agent doesn't answer, or nothing in the guest listens on the port |
| `timed-out` | It took longer than it was given |
| `in-use` | An image that machines are built on |

Treat a code you don't know as a failure. Newer versions may add codes.

### Progress

`create`, `image save` and `image import` write a line on standard error when a step starts, then at most ten a second:

```json
{"progress": "downloading", "received": 104857600, "total": 471756800}
```

| `progress` | Fields | Step |
|---|---|---|
| `downloading` | `received`, `total` (bytes; `total` may be `null`) | The image is downloading |
| `downloading-engine` | `received`, `total` | Linux only: cloud-hypervisor and its firmware, the first time |
| `writing-disk` | `fraction` (0 to 1) | The image is being made into a layer, once per image |
| `compressing` | `fraction` | A machine's disk is being compressed into an image |
| `importing` | `fraction` | An exported image is being read |

`wait` writes a line whenever the guest's state changes:

```json
{"progress": "guest", "booted": true, "setup": {"state": "running", "name": "xfce", "step": "Installing packages"}}
```

`setup` is `null` in a guest without setup scripts. Its `state` is `pending`, `running`, `finished` or `failed`. `name` and `step` may be missing.

Ignore any `progress` value you don't know.

## Objects

Sizes are in bytes and times in seconds since 1970. A field marked optional may be missing.

### Machine

```json
{"name": "a", "state": "running", "pid": 4242, "id": "8599c623…", "image": "debian-13", "arch": "arm64",
 "cpus": 2, "memory": 2147483648, "disk_size": 34359738368, "disk_used": 1060864,
 "disk_shared": 471756800, "user": "username", "created": 1795546459, "nested": false}
```

| Field | Meaning |
|---|---|
| `state` | `running`, `stopped` or `broken` |
| `pid` | Optional. The machine's process, while it runs |
| `problem` | Optional. Why a `broken` machine can't be read. The fields below `problem` mean nothing for a broken machine |
| `id` | Made when the machine is created. A later machine with the same name has another |
| `disk_size` | The disk's size as the guest sees it |
| `disk_used` | What the machine's own layer takes up on this computer |
| `disk_shared` | What the layers under it take up. Other machines share them |
| `nested` | The guest can run virtual machines of its own |

### Image

| Field | Meaning |
|---|---|
| `id` | What `create` takes |
| `name` | The system, for a catalog image. What it was made from, for the others |
| `kind` | `catalog`, `saved` (made with `image save`) or `automatic` (made by the first machine with setup scripts) |
| `cached` | It is on this computer |
| `size`, `user`, `created` | Optional. Only for images made from machines |

### Snapshot

`{"name": "one", "created": 1795546478, "size": 3555328}`. `size` is what the snapshot holds that the layers under it don't.

### Guest

What the agent in a running guest reports:

```json
{"agent": "0.1.0", "host_name": "a", "system": "Debian GNU/Linux 13 (trixie)", "kernel": "6.12.118+deb13-arm64",
 "arch": "aarch64", "addresses": [{"interface": "enp0s1", "address": "192.168.64.65"}], "booted": true}
```

`system` and `setup` (as in `wait`'s progress) are optional.

## Commands

| Command | Result |
|---|---|
| `version` | `{"name": "swiftvm", "version": "0.1.0", "api": 1}` |
| `images [--all]` | `{"images": [Image]}` |
| `create <image> ...` | `{"machine": Machine}`. Reports progress |
| `list` | `{"machines": [Machine]}`, sorted by name |
| `start <machine>` | `{"machine": Machine}` |
| `stop <machine>` | `{"machine": Machine}` |
| `delete <machine>` | `{"deleted": "<machine>"}` |
| `info <machine>` | `{"machine": Machine, "guest": Guest or null, "guest_error": Error or null}`. `guest` is `null` for a machine that isn't running. `guest_error` is set when the agent didn't answer |
| `wait <machine>` | `{"guest": Guest}`. Reports progress |
| `exec <machine> -- <command>` | `{"exit": {"status": 0}, "output": "...", "errors": "..."}`. `exit` has `status`, or `signal`, and may have `"timed_out": true` or `error`. Output is collected, then printed: use `ssh` for a stream |
| `cp <from> <to>` | `{"copied": {"from": "...", "to": "..."}}` |
| `snapshot <machine> <name>` | `{"snapshot": Snapshot}` |
| `snapshots <machine>` | `{"snapshots": [Snapshot]}` |
| `revert <machine> <name>` | `{"machine": Machine, "reverted": "<name>"}` |
| `delete-snapshot <machine> <name>` | `{"machine": Machine, "deleted": "<name>"}` |
| `image save <machine> <name>` | `{"image": Image}`. Reports progress |
| `image rename <image> <name>` | `{"image": Image}` |
| `image delete <image>` | `{"deleted": "<image>"}` |
| `image export <image> <file>` | `{"exported": "<image>", "file": "<file>"}` |
| `image import <file>` | `{"image": Image}`. Reports progress |
| `vnc <machine>` | `{"socket": "<path>"}`. See [The screen](#the-screen) |
| `screenshot <machine> [<file>]` | `{"path": "<file>", "width": 1280, "height": 800}`. With `-` as the file, the PNG goes to standard output and nothing else is printed |
| `click`, `move`, `drag`, `scroll`, `type`, `key`, `open` | `{"clicked": [x, y]}`, `{"moved": [x, y]}`, `{"dragged": {"from": [x, y], "to": [x, y]}}`, `{"scrolled": "down"}`, `{"typed": <characters>}`, `{"pressed": ["ctrl+s"]}`, `{"opened": "<what>"}` |
| `clipboard <machine> [<text>]` | `{"text": "..."}` |
| `disk info <machine>` | `{"machine": "<machine>", "layers": [...]}`: each layer's `path`, `hash`, `sealed`, `capacity`, `file_bytes`, `used_by`, `contents` |
| `disk check <machine>` | `{"layers": [{"path": "...", "problems": [...]}]}` |

`console`, `ssh`, `tunnel` and `mcp` connect a terminal or a program to the guest, and print no JSON.

### Long-running commands

**`watch`** prints one line at once and another whenever a machine is created, deleted, started or stopped, or anything else in its Machine changes. Each line is a full `list`:

```json
{"machines": [Machine, ...]}
```

Replace your list with each line. Changes to `disk_used` and `disk_shared` alone don't produce a line, so those two can be out of date. `watch` looks once a second: a state that lasts less than a second may never show. It ends within a second of its standard output being closed, or when it is signalled. If the store can't be read, it ends with an error document, as other commands do, and exit status 1: start it again.

**`forward <machine> <port>[:<guest port>]...`** prints `{"forwarding": [{"local": 8080, "guest": 80}]}` once it listens on `127.0.0.1`, then runs until it is signalled. Port 0 asks for any free port, and `local` says which one it got. A connection that can't reach the guest writes `{"error": Error}` on standard error.

## The screen

`vnc` gives the path of a Unix socket that speaks RFB 3.8 with no password. Only the user who owns the machine can open it. A client connects to it as it would to any VNC server. The socket exists while the machine runs.

## Where data lives

| Variable | Default on macOS | Default on Linux | Holds |
|---|---|---|---|
| `SWIFTVM_HOME` | `~/Library/Application Support/swiftvm` | `$XDG_DATA_HOME/swiftvm` or `~/.local/share/swiftvm` | Machines, images and layers |
| `SWIFTVM_CACHE` | `~/Library/Caches/swiftvm` | `$XDG_CACHE_HOME/swiftvm` or `~/.cache/swiftvm` | Downloaded images, and the engine on Linux |

Every program that uses the same folders sees the same machines. Several may run at once.

## Shipping the program in an app

- Each running machine is a process of the same program. Machine processes outlive the app. Replace the program by writing a new file and renaming it over the old one, never by writing into it: running machines keep the file they started from.
- On macOS the program needs the `com.apple.security.virtualization` entitlement. Re-signing it, for example with a Developer ID, has to keep that entitlement.
- On Linux, put `passt` next to the program, or rely on the system's. Its version must support `--vhost-user`.

## Changes to this API

Within version 1, a newer program may add fields to objects, codes to errors, values to `progress` and `state`, and new commands. It doesn't rename, remove or change the meaning of anything. A client should ignore what it doesn't know. Anything else is version 2.
