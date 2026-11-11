//! A consistent copy of the committed image, streamed while the tree runs
//! (plan 0019).

use std::collections::BTreeMap;
use std::io::Write;

use super::open::{select, tile};
use super::{Tree, TreeError};
use bf_tree_store::{COPY_BYTES, State};

impl Tree {
    /// Write the committed image to `out` as a store file, and return its
    /// committed state. Writers and checkpoints keep running.
    ///
    /// The output is the 8 KiB superblock area, then the committed page of
    /// every leaf, copied verbatim, so their leaf stamps and checksums stay
    /// valid. [`Tree::open`] of the output gives the tree as committed, and
    /// its `applied_seq` says where a log resumes.
    ///
    /// The committed state is read once, after a pin that keeps superseded
    /// pages from being freed, so the selection and the output superblock
    /// agree and no committed page is reused during the copy. Writers write
    /// only fresh pages, and selection ignores pages above the committed
    /// write watermark.
    pub fn write_snapshot(&self, out: &mut dyn Write) -> Result<State, TreeError> {
        let _pin = self.leaves.pin_frees();
        let store = self.leaves.store();
        let state = store.state();
        let selection = select(store, &self.config, state.write_seq)?;
        let chosen: BTreeMap<_, _> = selection
            .chosen
            .into_iter()
            .map(|(id, chosen)| (id.index(), chosen))
            .collect();
        // The output must open.
        tile(
            chosen
                .values()
                .map(|chosen| chosen.bounds.clone())
                .collect(),
        )?;

        let output = |error: std::io::Error| TreeError::SnapshotOutput {
            kind: error.kind(),
            code: error.raw_os_error(),
        };
        let mut block = vec![0; COPY_BYTES];
        store.superblock().encode(&state, &mut block)?;
        out.write_all(&block).map_err(output)?; // copy 0
        out.write_all(&block).map_err(output)?; // copy 1
        let mut page = vec![0; store.page_size()];
        for chosen in chosen.values() {
            store.read(chosen.page_number, &mut page)?;
            out.write_all(&page).map_err(output)?;
        }
        out.flush().map_err(output)?;
        Ok(state)
    }
}
