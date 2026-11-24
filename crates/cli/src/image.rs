//! Images made from machines (`image save`, `delete`, `rename`, `export`, `import`), and
//! snapshots of machines' disks.

use std::path::Path;

use host::{Host, ImageKind, ImageSummary, Result, SnapshotInfo};
use serde_json::json;

use crate::{Arguments, Done, Meter, invalid, print_table, size};

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
        let mut rows = vec![["IMAGE", "KIND", "WHAT", "STORED"].map(String::from).to_vec()];
        rows.extend(images.iter().map(|image| {
            let stored = match (image.kind, image.size) {
                (ImageKind::Catalog, _) => if image.cached { "yes" } else { "no" }.to_string(),
                (_, Some(bytes)) if image.cached => size(bytes),
                _ => "missing".to_string(),
            };
            vec![
                image.id.clone(),
                kind(image.kind).into(),
                image.name.clone(),
                stored,
            ]
        }));
        print_table(&rows);
    }
    Ok(json!({ "images": images }).into())
}

fn meter(json: bool) -> Meter {
    Meter::new(json)
}

fn said(json: bool, text: String) {
    if !json {
        println!("{text}");
    }
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
            said(
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
            said(json, format!("Deleted the image {image}."));
            Ok(json!({ "deleted": image }).into())
        }
        "rename" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (image, name) = pair(&arguments, "image", "new name")?;
            let renamed = host.rename_image(image, name)?;
            said(json, format!("Renamed the image {image} to {name}."));
            Ok(image_done(renamed))
        }
        "export" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let (image, file) = pair(&arguments, "image", "file to write")?;
            host.export_image(image, Path::new(file))?;
            said(json, format!("Exported the image {image} to {file}."));
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
            said(json, format!("Imported the image {}.", imported.id));
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
                println!("{machine} has no snapshots.");
            } else if !json {
                let mut rows = vec![["SNAPSHOT", "TAKEN", "SIZE"].map(String::from).to_vec()];
                rows.extend(snapshots.iter().map(|snapshot| {
                    vec![
                        snapshot.name.clone(),
                        crate::ago(snapshot.created),
                        size(snapshot.size),
                    ]
                }));
                print_table(&rows);
            }
            Ok(json!({ "snapshots": snapshots }).into())
        }
        "snapshot" => {
            let (machine, name) = pair(&arguments, "machine", "name for the snapshot")?;
            let taken = host.snapshot(machine, name)?;
            said(json, format!("Took the snapshot {name} of {machine}."));
            Ok(snapshot_json(&taken).into())
        }
        "revert" => {
            let (machine, name) = pair(&arguments, "machine", "snapshot")?;
            host.revert(machine, name)?;
            said(json, format!("Reverted {machine} to the snapshot {name}."));
            Ok(json!({ "machine": machine, "reverted": name }).into())
        }
        "delete-snapshot" => {
            let (machine, name) = pair(&arguments, "machine", "snapshot")?;
            host.delete_snapshot(machine, name)?;
            said(json, format!("Deleted the snapshot {name} of {machine}."));
            Ok(json!({ "machine": machine, "deleted": name }).into())
        }
        _ => unreachable!("only the snapshot commands come here"),
    }
}
