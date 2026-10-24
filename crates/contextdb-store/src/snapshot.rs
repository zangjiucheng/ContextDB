//! Input-layer snapshots (ADR 0006 §6): header, body, body hash. Format 3
//! bodies hold the whole kernel (ADR 0013 §8): distinct branch states, the
//! branch table and the sync points. A snapshot is only an accelerator;
//! anything that does not match the log is ignored.
use crate::durability::Durability;
use contextdb_core::{BranchInfo, Import, KernelSnapshot, KernelState};
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
    kernel: &KernelSnapshot,
) -> io::Result<()> {
    let snap = dir.join("snap");
    disk.create_dir_all(&snap)?;
    let body = encode(kernel);
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
fn parse(bytes: &[u8], checksums: &[ContentHash]) -> Option<(Epoch, KernelSnapshot)> {
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
    let kernel = decode(body).ok()?;
    (kernel.epoch == epoch).then_some((epoch, kernel))
}
/// A loaded snapshot: its epoch and the kernel's inputs.
pub type Loaded = (Epoch, KernelSnapshot);
fn encode(k: &KernelSnapshot) -> Vec<u8> {
    let mut o = Vec::new();
    k.epoch.encode(&mut o);
    (k.states.len() as u32).encode(&mut o);
    for s in &k.states {
        s.inputs.encode(&mut o);
        for list in &s.to_global {
            list.encode(&mut o);
        }
        s.unobserved.encode(&mut o);
    }
    (k.branches.len() as u32).encode(&mut o);
    for (info, state, imports) in &k.branches {
        info.id.encode(&mut o);
        info.name.encode(&mut o);
        info.parent.encode(&mut o);
        info.fork_epoch.encode(&mut o);
        info.archived.encode(&mut o);
        state.encode(&mut o);
        (imports.len() as u32).encode(&mut o);
        for (import, epoch) in imports {
            let (kind, id) = match import {
                Import::Logical(x) => (0u8, x.0),
                Import::Revision(x) => (1, x.0),
                Import::Rule(x) => (2, x.0),
                Import::Artifact(x) => (3, x.0),
            };
            kind.encode(&mut o);
            id.encode(&mut o);
            epoch.encode(&mut o);
        }
    }
    (k.syncs.len() as u32).encode(&mut o);
    for (src, dst, epoch, state) in &k.syncs {
        src.encode(&mut o);
        dst.encode(&mut o);
        epoch.encode(&mut o);
        state.encode(&mut o);
    }
    o
}
fn decode(body: &[u8]) -> Result<KernelSnapshot, WireError> {
    let mut i = Decoder::new(body, DecodeLimits::DURABLE);
    let epoch = Epoch::decode(&mut i)?;
    let mut states = Vec::new();
    for _ in 0..u32::decode(&mut i)? {
        let inputs = Inputs::decode(&mut i)?;
        let to_global = [
            Wire::decode(&mut i)?,
            Wire::decode(&mut i)?,
            Wire::decode(&mut i)?,
            Wire::decode(&mut i)?,
        ];
        states.push(KernelState {
            inputs,
            to_global,
            unobserved: Wire::decode(&mut i)?,
        });
    }
    let mut branches = Vec::new();
    for _ in 0..u32::decode(&mut i)? {
        let info = BranchInfo {
            id: Wire::decode(&mut i)?,
            name: Wire::decode(&mut i)?,
            parent: Wire::decode(&mut i)?,
            fork_epoch: Wire::decode(&mut i)?,
            archived: Wire::decode(&mut i)?,
        };
        let state = u32::decode(&mut i)?;
        let mut imports = Vec::new();
        for _ in 0..u32::decode(&mut i)? {
            let kind = u8::decode(&mut i)?;
            let id = u32::decode(&mut i)?;
            let import = match kind {
                0 => Import::Logical(LogicalId(id)),
                1 => Import::Revision(RevisionId(id)),
                2 => Import::Rule(RuleId(id)),
                3 => Import::Artifact(ArtifactId(id)),
                _ => return Err(WireError::InvalidEncoding),
            };
            imports.push((import, Epoch::decode(&mut i)?));
        }
        branches.push((info, state, imports));
    }
    let mut syncs = Vec::new();
    for _ in 0..u32::decode(&mut i)? {
        syncs.push((
            Wire::decode(&mut i)?,
            Wire::decode(&mut i)?,
            Wire::decode(&mut i)?,
            u32::decode(&mut i)?,
        ));
    }
    if i.remaining() != 0 {
        return Err(WireError::TrailingBytes);
    }
    Ok(KernelSnapshot {
        epoch,
        states,
        branches,
        syncs,
    })
}
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
