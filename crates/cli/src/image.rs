//! Images made from machines (`image save`, `delete`, `rename`, `export`, `import`), and
//! snapshots of machines' disks.

use std::path::Path;

use host::{Host, ImageKind, ImageSummary, Result, SnapshotInfo};
use serde_json::json;

use crate::ui::{Tone, Ui, success};
use crate::{Arguments, Done, Meter, invalid, plain, size};

/// The two things a command is about, such as a machine and a snapshot.
fn pair<'a>(arguments: &'a Arguments, first: &str, second: &str) -> Result<(&'a str, &'a str)> {
    match arguments.positional.as_slice() {
        [one, two] => Ok((one, two)),
        [] | [_] => Err(invalid(format!("Say which {first} and which {second}."))),
        [_, _, extra, ..] => Err(invalid(format!("\"{extra}\" isn't expected here."))),
    }
}

fn kind(kind: ImageKind) -> &'static str {
    match kind {
        ImageKind::Catalog => "catalog",
        ImageKind::Saved => "saved",
        ImageKind::Automatic => "automatic",
    }
}

/// `images`.
pub fn list(words: &[String], json: bool) -> Result<Done> {
    let arguments = Arguments::parse(words, &[], &["all"])?;
    arguments.nothing_else()?;
    let images = Host::open()?.images(arguments.flag("all"));
    if !json {
        let ui = Ui::stdout();
        let rows = images.iter().map(|image| {
            let stored = match (image.kind, image.size) {
                (ImageKind::Catalog, _) if image.cached => ui.paint(Tone::Good, "yes"),
                (ImageKind::Catalog, _) => ui.paint(Tone::Muted, "no"),
                (_, Some(bytes)) if image.cached => ui.paint(Tone::Good, &size(bytes)),
                _ => ui.paint(Tone::Warning, "missing"),
            };
            [
                ui.paint(Tone::Bold, &image.id),
                kind(image.kind).into(),
                plain(&image.name),
                stored,
            ]
        });
        ui.table(["IMAGE", "KIND", "DESCRIPTION", "STORED"], rows);
    }
    Ok(json!({ "images": images }).into())
}

fn meter(json: bool) -> Meter {
    Meter::new(json)
}

fn image_done(image: ImageSummary) -> Done {
    json!({ "image": image }).into()
}

/// `image <action> ...`.
pub fn run(words: &[String], json: bool) -> Result<Done> {
    let Some((action, words)) = words.split_first() else {
        return Err(invalid(
            "Say what to do with an image: save, delete, rename, export or import.",
        ));
    };
    let host = Host::open()?;
    match action.as_str() {
        "save" => {
            let arguments = Arguments::parse(words, &[], &["delete"])?;
            let (machine, image) = pair(&arguments, "machine", "name for the image")?;
            let mut meter = meter(json);
            let saved = host.save_image(machine, image, arguments.flag("delete"), &mut |progress| {
                meter.show(progress)
            });
            meter.finish();
            let saved = saved?;
            success(
                json,
                if arguments.flag("delete") {
                    format!("Saved {machine} as the image {image}, and deleted {machine}.")
                } else {
                    format!("Saved {machine} as the image {image}.")
                },
            );
            Ok(image_done(saved))
        }
        "delete" | "rm" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let image = arguments.subject("image")?;
            host.delete_image(image)?;
            success(json, format!("Deleted the image {image}."));
            Ok(json!({ "deleted": image }).into())
        }
        "rename" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (image, name) = pair(&arguments, "image", "new name")?;
            let renamed = host.rename_image(image, name)?;
            success(json, format!("Renamed the image {image} to {name}."));
            Ok(image_done(renamed))
        }
        "export" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (image, file) = pair(&arguments, "image", "file to write")?;
            host.export_image(image, Path::new(file))?;
            success(json, format!("Exported the image {image} to {file}."));
            Ok(json!({ "exported": image, "file": file }).into())
        }
        "import" => {
            let arguments = Arguments::parse(words, &["name"], &[])?;
            let file = arguments.subject("file")?;
            let name = arguments.value("name");
            let mut meter = meter(json);
            let imported = host.import_image(Path::new(file), name.as_deref(), &mut |progress| {
                meter.show(progress)
            });
            meter.finish();
            let imported = imported?;
            success(json, format!("Imported the image {}.", imported.id));
            Ok(image_done(imported))
        }
        _ => Err(invalid(format!(
            "An image has no command \"{action}\": save, delete, rename, export or import."
        ))),
    }
}

fn snapshot_json(snapshot: &SnapshotInfo) -> serde_json::Value {
    json!({ "snapshot": snapshot })
}

/// `snapshot`, `snapshots`, `revert` and `delete-snapshot`.
pub fn snapshots(command: &str, words: &[String], json: bool) -> Result<Done> {
    let arguments = Arguments::parse(words, &[], &[])?;
    let host = Host::open()?;
    match command {
        "snapshots" => {
            let machine = arguments.subject("machine")?;
            let snapshots = host.snapshots(machine)?;
            if !json && snapshots.is_empty() {
                println!("{} has no snapshots.", Ui::stdout().paint(Tone::Bold, machine));
            } else if !json {
                let ui = Ui::stdout();
                ui.table(
                    ["SNAPSHOT", "TAKEN", "SIZE"],
                    snapshots.iter().map(|snapshot| {
                        [
                            ui.paint(Tone::Bold, &snapshot.name),
                            crate::ago(snapshot.created),
                            size(snapshot.size),
                        ]
                    }),
                );
            }
            Ok(json!({ "snapshots": snapshots }).into())
        }
        "snapshot" => {
            let (machine, name) = pair(&arguments, "machine", "name for the snapshot")?;
            let taken = host.snapshot(machine, name)?;
            success(json, format!("Took the snapshot {name} of {machine}."));
            Ok(snapshot_json(&taken).into())
        }
        "revert" => {
            let (machine, name) = pair(&arguments, "machine", "snapshot")?;
            host.revert(machine, name)?;
            success(json, format!("Reverted {machine} to the snapshot {name}."));
            Ok(json!({ "machine": host.machine(machine)?, "reverted": name }).into())
        }
        "delete-snapshot" => {
            let (machine, name) = pair(&arguments, "machine", "snapshot")?;
            host.delete_snapshot(machine, name)?;
            success(json, format!("Deleted the snapshot {name} of {machine}."));
            Ok(json!({ "machine": host.machine(machine)?, "deleted": name }).into())
        }
        _ => unreachable!("only the snapshot commands come here"),
    }
}
