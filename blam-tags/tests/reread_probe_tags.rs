//! Can this engine re-read what it just wrote, for the files ManagedBlam
//! refuses?
//!
//! ManagedBlam rejects a from-definitions `particle` with
//! `cannot read tag struct 'particle_struct_definition', invalid chunk tag
//! ('' should be 'tgst')` - it walked the tag's own embedded layout, expected a
//! sub-chunk, and hit the end of the body. Two very different bugs produce that:
//!
//! - the writer emits fewer sub-chunks than the layout promises, in which case
//!   this engine's reader should fail too, or
//! - the file is self-consistent by this engine's rules and ManagedBlam demands
//!   a sub-chunk this engine does not, in which case only ManagedBlam complains.
//!
//! Set `BLAM_PROBE_OUT` to the directory `emit_probe_tags` wrote.

use std::path::PathBuf;

#[test]
#[ignore = "diagnostic"]
fn reread_what_managedblam_refused() {
    let Some(dir) = std::env::var_os("BLAM_PROBE_OUT").map(PathBuf::from) else {
        eprintln!("skipping: set BLAM_PROBE_OUT");
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipping: {} is not readable", dir.display());
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("def_"))
        })
        .collect();
    paths.sort();

    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        match std::panic::catch_unwind(|| blam_tags::TagFile::read(&path)) {
            Ok(Ok(tag)) => {
                // Touch every struct so a lazy failure cannot hide.
                let mut fields = 0usize;
                count(tag.root(), &mut fields);
                eprintln!("  OK      {name}  ({fields} fields)");
                if std::env::var_os("BLAM_PROBE_REFS").is_some() {
                    let mut refs = Vec::new();
                    references(tag.root(), "", &mut refs);
                    for line in refs {
                        eprintln!("            {line}");
                    }
                }
            }
            Ok(Err(error)) => eprintln!("  FAILED  {name}: {error}"),
            Err(_) => eprintln!("  PANIC   {name}"),
        }
    }
}

/// Every tag reference, with the group it claims and the path it points at.
fn references(value: blam_tags::TagStruct<'_>, prefix: &str, out: &mut Vec<String>) {
    for field in value.fields() {
        let key = blam_tags::convert::clean_field_key(field.name());
        let name = if key.is_empty() {
            field.type_name().to_owned()
        } else {
            key
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if let Some(blam_tags::TagFieldData::TagReference(reference)) = field.value() {
            match &reference.group_tag_and_name {
                Some((group, target)) if !target.is_empty() => out.push(format!(
                    "{path} -> [{}] {target}",
                    blam_tags::format_group_tag(*group)
                )),
                Some((group, _)) => out.push(format!(
                    "{path} -> [{}] <empty>",
                    blam_tags::format_group_tag(*group)
                )),
                None => out.push(format!("{path} -> <null>")),
            }
        }
        if let Some(child) = field.as_struct() {
            references(child, &path, out);
        }
        if let Some(block) = field.as_block() {
            for index in 0..block.len() {
                if let Some(element) = block.element(index) {
                    references(element, &format!("{path}[{index}]"), out);
                }
            }
        }
    }
}

fn count(value: blam_tags::TagStruct<'_>, total: &mut usize) {
    for field in value.fields() {
        *total += 1;
        if let Some(child) = field.as_struct() {
            count(child, total);
        }
        if let Some(block) = field.as_block() {
            for index in 0..block.len() {
                if let Some(element) = block.element(index) {
                    count(element, total);
                }
            }
        }
    }
}
