use bf_tree_circular_buffer::{CircularBuffer, CircularBufferFull, Kind};
use bf_tree_node_id::LeafId;
use bf_tree_page::{PageError, RecordState};

use super::{Apply, Mini, SizeClasses};

/// Slot directory bytes per record.
const SLOT_BYTES: usize = 8;

/// What [`buffer`] did with a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Buffered,
    /// A clean record met a dirty one for the same key; nothing changed.
    SkippedDirty,
    /// The mini-page is at the largest class and full. Nothing changed; the
    /// caller merges it into the base page and buffers again.
    NeedsMerge,
}

/// A record [`buffer`] could not take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BufferError {
    /// The circular buffer has no room. The leaf's logical content is
    /// unchanged.
    #[error(transparent)]
    Full(#[from] CircularBufferFull),
    #[error(transparent)]
    Page(#[from] PageError),
}

/// Buffer one record in the leaf's mini-page, creating and growing it as
/// needed.
///
/// A missing mini-page is allocated at the smallest class that holds the
/// record. A full one is compacted when that frees enough room, and
/// otherwise copied into the next class, dropping cold clean records. At the
/// largest class the result is [`WriteOutcome::NeedsMerge`]. `leaf` is both
/// the block's owner and the mini-page's backing leaf.
pub fn buffer(
    slot: &mut Option<Mini>,
    circular_buffer: &CircularBuffer,
    leaf: LeafId,
    classes: &SizeClasses,
    key: &[u8],
    state: RecordState<'_>,
) -> Result<WriteOutcome, BufferError> {
    if key.is_empty() {
        return Err(PageError::EmptyKey.into());
    }
    let record = SLOT_BYTES + key.len() + state.value().map_or(0, <[u8]>::len);
    let first_class = classes
        .smallest_for(record)
        .ok_or(PageError::RecordTooLarge)?;

    let fresh = slot.is_none();
    if fresh {
        let block = circular_buffer.alloc(first_class, Kind::Mini, leaf)?;
        *slot = Some(Mini::new(block, leaf));
    }

    loop {
        let mini = slot.as_mut().expect("the slot holds a mini-page");
        let error = match mini.apply(key, state) {
            Ok(Apply::Applied) => return Ok(WriteOutcome::Buffered),
            Ok(Apply::SkippedDirty) => return Ok(WriteOutcome::SkippedDirty),
            Err(error) => error,
        };

        match error {
            // Holes can make room without a copy.
            PageError::PageFull if mini.page().reclaimable_bytes() > 0 => {
                mini.compact();
            }
            // `RecordTooLarge` means the record does not fit even an empty page
            // of this class; only growth helps.
            PageError::PageFull | PageError::RecordTooLarge => {
                let capacity = mini.page().capacity();
                let Some(next) = classes.next(capacity) else {
                    return Ok(WriteOutcome::NeedsMerge);
                };
                // Allocate before taking the old mini-page out, so a full
                // circular buffer leaves the slot intact.
                let block = circular_buffer.alloc(next, Kind::Mini, leaf)?;
                let mut grown = Mini::new(block, leaf);
                mini.copy_into(&mut grown, true);
                let old = slot.replace(grown).expect("the slot held a mini-page");
                circular_buffer.free(old.block);
            }
            error => {
                if fresh {
                    let mini = slot.take().expect("allocated above");
                    circular_buffer.free(mini.block);
                }
                return Err(error.into());
            }
        }
    }
}
