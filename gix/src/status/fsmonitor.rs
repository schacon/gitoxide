//! Support for Git's builtin filesystem monitor, `git fsmonitor--daemon`.
//!
//! When `core.fsmonitor=true` and the index carries a token from a previous query, we ask the daemon
//! which paths changed since then. All other index entries are then known to be unchanged and are marked
//! with [`FSMONITOR_VALID`](gix_index::entry::Flags::FSMONITOR_VALID) so the status skips their `lstat()`.
//!
//! If the index also has Git's untracked cache, the listings of directories the daemon didn't report
//! are reused instead of reading these directories again.
//!
//! Set `GIX_FSMONITOR=0` to disable this entirely, or `GIX_FSMONITOR=tracked` to only skip tracked files,
//! which helps comparing results with and without it.
use std::collections::HashSet;

use crate::bstr::{BStr, BString, ByteSlice};

/// Indices with fewer entries are faster to check without asking the daemon.
const MIN_INDEX_ENTRIES: usize = 2000;

/// The result of a successful fsmonitor query.
pub(crate) struct Refreshed {
    /// A copy of the index with `FSMONITOR_VALID` set on all entries known to be unchanged.
    pub index: gix_index::File,
    /// Directory listings known to be unchanged, if the untracked cache could be used.
    pub cached_listing: Option<gix_dir::walk::CachedListing>,
}

/// Query the fsmonitor daemon of `repo` and apply its answer to a copy of `index`.
///
/// Returns `None` if the fsmonitor isn't configured or usable, or if it answered that everything may have changed,
/// in which case the status has to look at everything as usual.
pub(crate) fn refresh(repo: &crate::Repository, index: &gix_index::File) -> Option<Refreshed> {
    let _span = gix_trace::coarse!("gix::status::fsmonitor::refresh");
    let mode = std::env::var_os("GIX_FSMONITOR");
    if mode.as_deref() == Some("0".as_ref()) {
        return None;
    }
    let use_untracked_cache = mode.as_deref() != Some("tracked".as_ref());
    if !matches!(repo.config.resolved.boolean("core.fsmonitor"), Ok(Some(true))) {
        return None;
    }
    // Asking the daemon costs a round trip in which it syncs with the filesystem's event stream,
    // about 10ms on macOS, which is more than checking a small index takes.
    if index.entries().len() < MIN_INDEX_ENTRIES {
        return None;
    }
    let fsmonitor = index.fs_monitor()?;
    let gix_index::extension::fs_monitor::Token::V2 { token } = fsmonitor.token() else {
        return None;
    };
    let response = match query_daemon(&repo.git_dir().join("fsmonitor--daemon.ipc"), token.as_ref()) {
        Ok(response) => response,
        Err(err) => {
            gix_trace::debug!(?err, "fsmonitor daemon could not be queried");
            return None;
        }
    };
    let Some(changed) = parse_response(&response) else {
        gix_trace::debug!("fsmonitor daemon answered that anything may have changed");
        return None;
    };
    gix_trace::debug!(changed_paths = changed.len(), "fsmonitor daemon answered");

    let mut index = index.clone();
    {
        let state: &mut gix_index::State = &mut index;
        let num_entries = state.entries().len();
        if fsmonitor.entry_dirty().num_bits() > num_entries {
            return None;
        }
        for entry in state.entries_mut() {
            entry.flags.insert(gix_index::entry::Flags::FSMONITOR_VALID);
        }
        let mut invalid = Vec::new();
        fsmonitor.entry_dirty().for_each_set_bit(|idx| {
            invalid.push(idx);
            Some(())
        });
        for path in &changed {
            if let Some(range) = state.prefixed_entries_range(path) {
                for idx in range {
                    let entry_path = state.entries()[idx].path(state);
                    if entry_path[..] == path[..] || entry_path.get(path.len()) == Some(&b'/') {
                        invalid.push(idx);
                    }
                }
            }
        }
        let entries = state.entries_mut();
        for idx in invalid {
            if let Some(entry) = entries.get_mut(idx) {
                entry.flags.remove(gix_index::entry::Flags::FSMONITOR_VALID);
            }
        }
    }

    let cached_listing = use_untracked_cache
        .then(|| untracked_listing(repo, &index, &changed))
        .flatten();
    Some(Refreshed { index, cached_listing })
}

/// Turn Git's untracked cache into listings of all directories that the fsmonitor didn't report as changed.
fn untracked_listing(
    repo: &crate::Repository,
    index: &gix_index::State,
    changed: &[&BStr],
) -> Option<gix_dir::walk::CachedListing> {
    let cache = index.untracked()?;
    let workdir = repo.workdir()?;
    // The cache is only valid for the worktree it was written for.
    if !cache
        .identifier()
        .contains_str(gix_path::into_bstr(workdir).ok()?.as_ref())
    {
        return None;
    }
    // A changed exclude file can change what's untracked anywhere, so don't try to be smart about it.
    let exclude_file_name = cache.exclude_filename_per_dir();
    if changed
        .iter()
        .any(|path| path.rsplit_str("/").next() == Some(exclude_file_name.as_bytes()))
    {
        return None;
    }
    let info_exclude = repo.common_dir().join("info").join("exclude");
    match (cache.info_exclude(), std::fs::symlink_metadata(&info_exclude).ok()) {
        (None, None) => {}
        (Some(cached), Some(current)) if stat_matches(cached.stat(), &current) => {}
        _ => {
            gix_trace::debug!("untracked cache is outdated: `info/exclude` changed");
            return None;
        }
    }

    let mut invalid_dirs = HashSet::<&BStr>::new();
    for &path in changed {
        invalid_dirs.insert(path);
        invalid_dirs.insert(path.rfind_byte(b'/').map_or("".into(), |pos| path[..pos].as_bstr()));
    }

    let directories = cache.directories();
    let mut listing = gix_dir::walk::CachedListing::default();
    let mut stack = vec![(0_usize, BString::default())];
    while let Some((dir_idx, dir_path)) = stack.pop() {
        let dir = directories.get(dir_idx)?;
        let is_valid = dir.stat().is_some() && !dir.check_only() && !invalid_dirs.contains(dir_path.as_bstr());
        let mut entries = Vec::new();
        if is_valid {
            for name in dir.untracked_entries() {
                if let Some(dir_name) = name.strip_suffix(b"/") {
                    entries.push((dir_name.into(), gix_dir::entry::Kind::Directory));
                } else {
                    let mut path = workdir.join(gix_path::from_bstr(dir_path.as_bstr()).ok()?);
                    path.push(gix_path::from_bstr(name.as_bstr()).ok()?);
                    let kind = std::fs::symlink_metadata(&path)
                        .ok()
                        .map_or(gix_dir::entry::Kind::File, |md| md.file_type().into());
                    entries.push((name.clone(), kind));
                }
            }
        }
        for &sub_idx in dir.sub_directories() {
            let sub = directories.get(sub_idx)?;
            if is_valid && !entries.iter().any(|(name, _)| name.as_bstr() == sub.name()) {
                entries.push((sub.name().to_owned(), gix_dir::entry::Kind::Directory));
            }
            let mut sub_path = dir_path.clone();
            if !sub_path.is_empty() {
                sub_path.push(b'/');
            }
            sub_path.extend_from_slice(sub.name());
            stack.push((sub_idx, sub_path));
        }
        if is_valid {
            listing.directories.insert(dir_path, entries);
        }
    }
    gix_trace::debug!(listings = listing.directories.len(), "untracked cache directory listings reused");
    Some(listing)
}

fn stat_matches(cached: &gix_index::entry::Stat, current: &std::fs::Metadata) -> bool {
    let Ok(mtime) = current.modified() else { return false };
    let Ok(mtime) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    u64::from(cached.size) == current.len() & 0xffff_ffff
        && u64::from(cached.mtime.secs) == mtime.as_secs()
        && cached.mtime.nsecs == mtime.subsec_nanos()
}

/// Return the paths that changed, or `None` if the daemon answered that everything may have changed.
fn parse_response(response: &[u8]) -> Option<Vec<&BStr>> {
    let mut parts = response.split(|b| *b == 0);
    let _new_token = parts.next()?;
    let mut changed = Vec::new();
    for path in parts {
        if path.is_empty() {
            continue;
        }
        if path == b"/" {
            return None;
        }
        changed.push(path.strip_suffix(b"/").unwrap_or(path).as_bstr());
    }
    Some(changed)
}

/// Send `token` to the daemon listening on `socket` using Git's "simple IPC" protocol, and return its answer.
#[cfg(unix)]
fn query_daemon(socket: &std::path::Path, token: &BStr) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Write};
    const MAX_PKT_DATA: usize = 65516;

    // Socket addresses are limited to about 100 bytes, so fall back to a path relative to the current directory,
    // similar to what Git does.
    let mut stream = match std::os::unix::net::UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidInput => {
            let cwd = std::env::current_dir()?;
            let relative = socket.strip_prefix(&cwd).map_err(|_| err)?;
            std::os::unix::net::UnixStream::connect(relative)?
        }
        Err(err) => return Err(err),
    };
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut message = Vec::with_capacity(token.len() + 8);
    for chunk in token.chunks(MAX_PKT_DATA) {
        write!(message, "{:04x}", chunk.len() + 4)?;
        message.extend_from_slice(chunk);
    }
    message.extend_from_slice(b"0000");
    stream.write_all(&message)?;

    let mut response = Vec::new();
    loop {
        let mut len = [0_u8; 4];
        stream.read_exact(&mut len)?;
        let len = std::str::from_utf8(&len)
            .ok()
            .and_then(|len| usize::from_str_radix(len, 16).ok())
            .ok_or_else(|| std::io::Error::other("invalid pkt-line length"))?;
        if len == 0 {
            break;
        }
        let data_len = len
            .checked_sub(4)
            .ok_or_else(|| std::io::Error::other("invalid pkt-line length"))?;
        let start = response.len();
        response.resize(start + data_len, 0);
        stream.read_exact(&mut response[start..])?;
    }
    Ok(response)
}

#[cfg(not(unix))]
fn query_daemon(_socket: &std::path::Path, _token: &BStr) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other("fsmonitor IPC is only implemented for unix sockets"))
}
