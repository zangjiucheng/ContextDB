//! Input-layer snapshots (ADR 0006 §6): header, Wire(Inputs) body, body hash.
//! A snapshot is only an accelerator; anything that does not match the log is ignored.
use crate::durability::Durability;
use contextdb_model::*;
use std::{io, path::Path};

const MAGIC: &[u8; 8] = b"CDBSNAP\x01";
/// magic 8 + format 4 + epoch 8 + log checksum 32 + body length 8.
const HEADER: usize = 60;

fn name(epoch: Epoch) -> String {
    format!("snap-{:020}.bin", epoch.0)
}
pub fn write(
    disk: &dyn Durability,
    dir: &Path,
    epoch: Epoch,
    checksum: &ContentHash,
    inputs: &Inputs,
) -> io::Result<()> {
    let snap = dir.join("snap");
    disk.create_dir_all(&snap)?;
    let body = inputs.canonical_bytes();
    let mut out = Vec::with_capacity(HEADER + body.len() + 32);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&crate::log::FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&epoch.0.to_le_bytes());
    out.extend_from_slice(checksum);
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(blake3::hash(&body).as_bytes());
    let tmp = snap.join(format!("{}.tmp", name(epoch)));
    disk.write_new(&tmp, &out)?;
    disk.sync(&tmp)?;
    disk.rename(&tmp, &snap.join(name(epoch)))?;
    disk.sync_dir(&snap)?;
    disk.sync_dir(dir)
}
fn parse(bytes: &[u8], checksums: &[ContentHash]) -> Option<(Epoch, Inputs)> {
    if bytes.len() < HEADER + 32 || &bytes[..8] != MAGIC {
        return None;
    }
    let format = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let epoch = Epoch(u64::from_le_bytes(bytes[12..20].try_into().ok()?));
    let checksum: ContentHash = bytes[20..52].try_into().ok()?;
    let len = usize::try_from(u64::from_le_bytes(bytes[52..60].try_into().ok()?)).ok()?;
    if format != crate::log::FORMAT_VERSION || bytes.len() != HEADER + len + 32 {
        return None;
    }
    // The snapshot must name a verified frame's checksum (epoch 0: genesis).
    let expected = match epoch.0 {
        0 => crate::log::GENESIS,
        n => *checksums.get(usize::try_from(n - 1).ok()?)?,
    };
    let body = &bytes[HEADER..HEADER + len];
    if checksum != expected || blake3::hash(body).as_bytes() != &bytes[HEADER + len..] {
        return None;
    }
    let inputs = Inputs::from_bytes_with(body, DecodeLimits::DURABLE).ok()?;
    Some((epoch, inputs))
}
/// A loaded snapshot: its epoch and input layer.
pub type Loaded = (Epoch, Inputs);
/// The accepted snapshot (if any) and the names of rejected snapshot files.
pub type Newest<T> = (Option<(Epoch, T)>, Vec<String>);
/// Newest snapshot consistent with the verified log, plus names of rejected files.
/// Leftover temp files are removed.
/// Newest snapshot that matches the log and that `build` accepts; a snapshot
/// whose body verifies but which `build` rejects is ignored like a corrupt
/// one, since snapshots are only an accelerator (ADR 0006 §6).
pub fn load_newest<T>(
    disk: &dyn Durability,
    dir: &Path,
    checksums: &[ContentHash],
    mut build: impl FnMut(Loaded) -> Option<T>,
) -> io::Result<Newest<T>> {
    let snap = dir.join("snap");
    if !disk.exists(&snap) {
        return Ok((None, Vec::new()));
    }
    let mut rejected = Vec::new();
    let mut names = disk.list(&snap)?;
    names.sort();
    for n in names.iter().filter(|n| n.ends_with(".tmp")) {
        disk.remove(&snap.join(n))?;
    }
    for n in names.iter().rev().filter(|n| n.ends_with(".bin")) {
        let found = parse(&disk.read(&snap.join(n))?, checksums).and_then(|loaded| {
            let epoch = loaded.0;
            build(loaded).map(|t| (epoch, t))
        });
        match found {
            Some(found) => return Ok((Some(found), rejected)),
            None => rejected.push(n.clone()),
        }
    }
    Ok((None, rejected))
}
