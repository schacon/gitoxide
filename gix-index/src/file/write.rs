use crate::{File, State, Version, extension, write};
use gix_error::Result;

impl File {
    /// Write the index to `out` with `options`, to be readable by [`File::at()`], returning the version that was actually written
    /// to retain all information of this index.
    ///
    /// Note that the `tree` (tree-cache) extension is written as-is and is **not** recomputed or
    /// invalidated to match the current entries; see [`File::write()`] for the implications and the
    /// recommended workaround.
    pub fn write_to(
        &self,
        mut out: impl std::io::Write,
        options: write::Options,
    ) -> Result<(Version, gix_hash::ObjectId)> {
        let _span = gix_trace::detail!("gix_index::File::write_to()", skip_hash = options.skip_hash);
        let (version, hash) = if options.skip_hash {
            let out: &mut dyn std::io::Write = &mut out;
            let version = self.state.write_to(out, options)?;
            (version, self.state.object_hash.null())
        } else {
            let mut hasher = gix_hash::io::Write::new(&mut out, self.state.object_hash);
            let out: &mut dyn std::io::Write = &mut hasher;
            let version = self.state.write_to(out, options)?;
            (version, hasher.hash.try_finalize()?)
        };
        out.write_all(hash.as_slice()).map_err(gix_hash::io::from_std_io)?;
        Ok((version, hash))
    }

    /// Write ourselves to the path we were read from after acquiring a lock, using `options`.
    ///
    /// Note that the hash produced will be stored which is why we need to be mutable.
    ///
    /// ### The `tree` (tree-cache) extension is written as-is
    ///
    /// The `tree` extension (tree-cache) is serialized from its current in-memory state; it is
    /// **not** recomputed or invalidated to match the entries. So if entries were modified since the
    /// index was read, the tree-cache is written back still marked valid even though it is now stale.
    ///
    /// Git uses the tree-cache to skip unchanged directories when building a tree (on `git commit` /
    /// `git write-tree`), so a stale-but-valid tree-cache can make a later commit capture outdated
    /// subtree content; more generally, `git status` and later commits can disagree about what is
    /// staged.
    ///
    /// Until the tree-cache is updated on write (see [issue #2421]), remove it with
    /// [`State::remove_tree()`](crate::State::remove_tree()) before writing whenever entries were
    /// changed:
    ///
    /// ```ignore
    /// index.remove_tree();
    /// index.write(gix_index::write::Options::default())?;
    /// ```
    ///
    /// [issue #2421]: https://github.com/GitoxideLabs/gitoxide/issues/2421
    pub fn write(&mut self, options: write::Options) -> Result {
        use gix_error::{ErrorExt, ResultExt, bail, message};

        let _span = gix_trace::detail!("gix_index::File::write()", path = ?self.path);
        self.carry_over_fsmonitor_data();
        let mut lock = std::io::BufWriter::with_capacity(
            64 * 1024,
            gix_lock::File::acquire_to_update_resource(&self.path, gix_lock::acquire::Fail::Immediately, None, 0)
                .or_raise(|| message("Could not acquire lock for index file"))?,
        );
        let (version, digest) = self
            .write_to(&mut lock, options)
            .or_raise(|| message("Could not write index"))?;
        match lock.into_inner() {
            Ok(lock) => lock
                .commit()
                .or_raise(|| message("Could not commit lock for index file"))?,
            Err(err) => {
                bail!(
                    err.into_error()
                        .and_raise(message("Could not flush buffered index data"))
                );
            }
        };
        self.state.version = version;
        self.checksum = Some(digest);
        Ok(())
    }
}

impl File {
    /// If the index we are about to replace has Git's fsmonitor and untracked-cache extensions, carry them over
    /// so the filesystem monitor keeps working for us and for Git.
    fn carry_over_fsmonitor_data(&mut self) {
        let Ok(data) = std::fs::read(&self.path) else {
            return;
        };
        if !data.windows(4).any(|w| w == extension::fs_monitor::SIGNATURE) {
            return;
        }
        let Ok((old, _)) = State::from_bytes(
            &data,
            filetime::FileTime::now(),
            self.state.object_hash,
            crate::decode::Options::default(),
        ) else {
            return;
        };
        self.carry_over_fsmonitor_data_from(&old);
    }

    /// Take Git's fsmonitor token and untracked cache over from `old`, the index this one replaces,
    /// adjusted to this index's entries.
    ///
    /// An entry stays known-valid only if its path, stage, id, mode and stat are unchanged and it was
    /// valid in `old`. Directories of paths that were added to or removed from the index lose their
    /// cached untracked listing, like Git does it.
    ///
    /// [`write()`](Self::write()) does this with the index it overwrites. Call it before writing when
    /// that index was already replaced by a tool that doesn't keep these extensions.
    pub fn carry_over_fsmonitor_data_from(&mut self, old: &State) {
        let Some(old_fsm) = old.fs_monitor() else {
            return;
        };
        let mut old_dirty = vec![false; old.entries().len()];
        old_fsm.entry_dirty().for_each_set_bit(|idx| {
            if let Some(bit) = old_dirty.get_mut(idx) {
                *bit = true;
            }
            Some(())
        });
        let new_dirty: Vec<bool> = self
            .state
            .entries()
            .iter()
            .map(|entry| {
                old.entry_index_by_path_and_stage(entry.path(&self.state), entry.stage())
                    .is_none_or(|idx| {
                        let old_entry = &old.entries()[idx];
                        old_dirty[idx]
                            || old_entry.stat != entry.stat
                            || old_entry.id != entry.id
                            || old_entry.mode != entry.mode
                    })
            })
            .collect();
        let Some(entry_dirty) = gix_bitmap::ewah::Vec::from_bits(&new_dirty) else {
            return;
        };
        self.state.fs_monitor = Some(extension::FsMonitor::new(old_fsm.token().clone(), entry_dirty));

        self.state.untracked = old.untracked().cloned().map(|mut untracked| {
            for entry in self.state.entries() {
                let path = entry.path(&self.state);
                if old.entry_index_by_path(path).is_err() {
                    untracked.invalidate_path(path);
                }
            }
            for entry in old.entries() {
                let path = entry.path(&old);
                if self.state.entry_index_by_path(path).is_err() {
                    untracked.invalidate_path(path);
                }
            }
            untracked
        });
    }
}
