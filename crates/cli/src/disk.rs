//! `disk info` and `disk check`: the layers of a machine's disk.

use host::{Host, LAYER_CLUSTER, LayerInfo, Result};
use serde_json::json;

use crate::ui::{Tone, Ui};
use crate::{Arguments, Done, invalid, plain, size};

/// `bottom` is a layer that is built on nothing.
fn describe(layer: &LayerInfo, bottom: bool) -> Vec<(&'static str, String)> {
    let mut rows = vec![
        ("File", plain(&layer.path.display().to_string())),
        ("Size", size(layer.capacity)),
        ("On disk", size(layer.file_bytes)),
        ("Sealed", if layer.sealed { "yes" } else { "no" }.to_string()),
    ];
    if layer.sealed {
        rows.push(("Used by", layer.used_by.to_string()));
    }
    match &layer.contents {
        Some(contents) => {
            rows.push((
                "Clusters",
                format!(
                    "{} stored ({} packed from {}), {} zeros, {} {}, of {}",
                    contents.stored,
                    size(contents.stored_bytes),
                    size(contents.stored * LAYER_CLUSTER as u64),
                    contents.zeros,
                    contents.inherited,
                    if bottom { "empty" } else { "from underneath" },
                    contents.clusters
                ),
            ));
        }
        None => rows.push(("Clusters", "not read while the machine is running".into())),
    }
    rows
}

pub fn run(words: &[String], json: bool) -> Result<Done> {
    let Some((action, words)) = words.split_first() else {
        return Err(invalid("Say what to do with the disk: info or check."));
    };
    match action.as_str() {
        "info" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let info = Host::open()?.disk_info(arguments.subject("machine or layer")?)?;
            if !json {
                let ui = Ui::stdout();
                for (index, layer) in info.layers.iter().enumerate() {
                    if index > 0 {
                        println!();
                    }
                    let title = match (&layer.hash, index) {
                        (_, 0) if info.machine.is_some() => "Own layer".to_string(),
                        (_, 0) => "Layer".to_string(),
                        (Some(hash), _) => format!("Built on {}", &hash[..12]),
                        (None, _) => "Built on".to_string(),
                    };
                    println!("{}", ui.paint(Tone::Heading, &title));
                    let bottom = index + 1 == info.layers.len();
                    println!("{}", ui.pairs(&describe(layer, bottom)));
                }
            }
            Ok(json!(info).into())
        }
        "check" => {
            let arguments = Arguments::parse(words, &[], &[])?;
            let check = Host::open()?.check_disk(arguments.subject("machine or layer")?)?;
            if !json {
                let ui = Ui::stdout();
                for layer in &check.layers {
                    let path = ui.paint(Tone::Bold, &layer.path.display().to_string());
                    if layer.problems.is_empty() {
                        println!("{path}: {}", ui.paint(Tone::Good, "whole"));
                    } else {
                        println!(
                            "{path}: {}",
                            ui.paint(Tone::Error, &format!("{} problems", layer.problems.len()))
                        );
                        for problem in layer.problems.iter().take(20) {
                            println!("  {}", plain(problem));
                        }
                    }
                }
            }
            Ok(Done {
                status: if check.is_whole() { 0 } else { 1 },
                result: json!(check),
            })
        }
        _ => Err(invalid(format!(
            "The disk has no command \"{action}\": info or check."
        ))),
    }
}
