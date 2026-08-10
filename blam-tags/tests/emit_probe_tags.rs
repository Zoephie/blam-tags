//! Emit freshly converted tags so the editing kit's own ManagedBlam can be
//! asked to open them.
//!
//! Foundation's crash is not reachable from inside this engine: it happens in
//! `Corinth.Tags.TagFieldStringMenuItem.PopulateSubMenuItems`, building the
//! dropdown for a field whose options come from *another tag*. The only way to
//! observe it is to hand a real file to real ManagedBlam, so this writes the
//! files and a companion probe drives the kit.
//!
//! Output directory comes from `BLAM_PROBE_OUT`; nothing is written without it,
//! so a normal test run never touches a kit.

use blam_tags::convert::*;
use std::path::PathBuf;

fn kit(name: &str, folder: &str) -> Option<PathBuf> {
    let path = PathBuf::from("D:/SteamLibrary/steamapps/common")
        .join(name)
        .join(folder);
    path.is_dir().then_some(path)
}

/// Bisect: which stage produces a file ManagedBlam will not open?
///
/// - `rt_*`  a shipped H4 tag read and written straight back out. Only the
///           writer is involved - no conversion, no built layout, and the
///           layout is the kit's own. If these fail, the writer is at fault.
/// - `new_*` `TagFile::new` from the definitions, written with no conversion.
///           Only the layout builder is involved. If `rt_*` passes and these
///           fail, the built layout is at fault.
///
/// `def_*` from the other test adds conversion on top of `new_*`.
#[test]
#[ignore = "diagnostic"]
fn emit_bisect_tags_for_managedblam() {
    let Some(out) = std::env::var_os("BLAM_PROBE_OUT").map(PathBuf::from) else {
        eprintln!("skipping: set BLAM_PROBE_OUT");
        return;
    };
    let Some(h4) = kit("H4EK", "tog").or_else(|| kit("H4EK", "tags")) else {
        eprintln!("skipping: needs H4EK");
        return;
    };
    let definitions = PathBuf::from("../../blam-tag-gui/definitions");
    std::fs::create_dir_all(&out).expect("probe output directory");
    let h4_files = walk_files(&h4);

    for group in ["particle", "tracer_system", "effect", "decal_system"] {
        // Round-trip a shipped tag: writer only.
        let shipped = h4_files
            .iter()
            .find(|path| path.extension().and_then(|e| e.to_str()) == Some(group));
        match shipped {
            Some(path) => match blam_tags::TagFile::read(path) {
                Ok(tag) => match tag.write_to_bytes() {
                    Ok(bytes) => {
                        let file = out.join(format!("rt_{group}.{group}"));
                        std::fs::write(&file, &bytes).expect("write round-trip tag");
                        let original = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        eprintln!(
                            "rt_{group}: {} bytes out, {original} in, byte-identical={}",
                            bytes.len(),
                            std::fs::read(path).is_ok_and(|source| source == bytes)
                        );
                    }
                    Err(error) => eprintln!("rt_{group}: write failed: {error}"),
                },
                Err(error) => eprintln!("rt_{group}: read failed: {error}"),
            },
            None => eprintln!("rt_{group}: H4EK ships none"),
        }

        // Straight from the definitions: layout builder only.
        let schema = definitions.join("halo4_mcc").join(format!("{group}.json"));
        if !schema.is_file() {
            eprintln!("new_{group}: no halo4_mcc definition");
            continue;
        }
        match std::panic::catch_unwind(|| blam_tags::TagFile::new(&schema)) {
            Ok(Ok(mut tag)) => {
                if let Err(error) = apply_editing_kit_mcc_header(&mut tag, "halo4_mcc") {
                    eprintln!("new_{group}: header stamp failed: {error}");
                    continue;
                }
                match tag.write_to_bytes() {
                    Ok(bytes) => {
                        let file = out.join(format!("new_{group}.{group}"));
                        std::fs::write(&file, &bytes).expect("write built tag");
                        eprintln!("new_{group}: {} bytes", bytes.len());
                    }
                    Err(error) => eprintln!("new_{group}: write failed: {error}"),
                }
            }
            Ok(Err(error)) => eprintln!("new_{group}: build failed: {error}"),
            Err(_) => eprintln!("new_{group}: panicked"),
        }
    }
}

#[test]
#[ignore = "diagnostic"]
fn emit_converted_fx_tags_for_managedblam() {
    let Some(out) = std::env::var_os("BLAM_PROBE_OUT").map(PathBuf::from) else {
        eprintln!("skipping: set BLAM_PROBE_OUT to the directory to write into");
        return;
    };
    let Some(reach) = kit("HREK", "tags") else {
        eprintln!("skipping: needs HREK");
        return;
    };
    let definitions = PathBuf::from("../../blam-tag-gui/definitions");
    if !definitions.is_dir() {
        eprintln!("skipping: no definitions tree beside the engine");
        return;
    }
    std::fs::create_dir_all(&out).expect("probe output directory");

    let files = walk_files(&reach);
    // Both starting points, so a difference between them is visible to the kit.
    for (label, from_definitions) in [("kit", false), ("def", true)] {
        if from_definitions {
            unsafe { std::env::set_var("BLAM_BUILD_FROM_DEFINITIONS", "1") };
        } else {
            unsafe { std::env::remove_var("BLAM_BUILD_FROM_DEFINITIONS") };
        }
        for (group, fourcc) in [
            ("effect", "effe"),
            ("particle", "prt3"),
            ("contrail_system", "cntl"),
            ("decal_system", "decs"),
        ] {
            let Some(group_tag) = blam_tags::parse_group_tag(fourcc) else {
                eprintln!("{group}: unknown four-cc");
                continue;
            };
            let mut written = 0usize;
            for path in files.iter() {
                if path.extension().and_then(|e| e.to_str()) != Some(group) {
                    continue;
                }
                let Ok(source) = read_tag_for_conversion(
                    path,
                    Some("haloreach_mcc"),
                    Some(&definitions),
                    group_tag,
                ) else {
                    continue;
                };
                let result = std::panic::catch_unwind(|| {
                    analyze_conversion_with_templates(
                        &source,
                        "haloreach_mcc",
                        "halo4_mcc",
                        &definitions,
                        None,
                    )
                });
                let Ok(Ok(mut draft)) = result else { continue };
                if apply_editing_kit_mcc_header(&mut draft.tag, "halo4_mcc").is_err() {
                    continue;
                }
                let Ok(bytes) = draft.tag.write_to_bytes() else {
                    continue;
                };
                let stem = path.file_stem().unwrap().to_string_lossy().to_string();
                let name = format!("{label}_{group}_{written}_{stem}");
                let file = out.join(format!("{name}.{}", draft.target_extension));
                std::fs::write(&file, &bytes).expect("write probe tag");
                eprintln!(
                    "wrote {}  ({} bytes, from {})",
                    file.file_name().unwrap().to_string_lossy(),
                    bytes.len(),
                    path.display()
                );
                written += 1;
                if written >= 3 {
                    break;
                }
            }
            if written == 0 {
                eprintln!("{label} {group}: nothing converted");
            }
        }
    }
    unsafe { std::env::remove_var("BLAM_BUILD_FROM_DEFINITIONS") };
}
