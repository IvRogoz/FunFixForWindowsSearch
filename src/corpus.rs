use std::collections::HashMap;

use crate::SearchItem;

pub(crate) fn item_memory_bytes(item: &SearchItem) -> usize {
    std::mem::size_of::<SearchItem>() + item.path.len()
}

pub(crate) fn item_positions(items: &[SearchItem]) -> HashMap<Box<str>, usize> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| (item.path.clone(), index))
        .collect()
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct DeltaStats {
    pub(crate) added: usize,
    pub(crate) updated: usize,
    pub(crate) deleted: usize,
    pub(crate) bytes_added: usize,
    pub(crate) bytes_removed: usize,
}

// The lookup survives batches, so applying a change never scans the full corpus.
pub(crate) fn apply_delta(
    items: &mut Vec<SearchItem>,
    positions: &mut HashMap<Box<str>, usize>,
    upserts: Vec<SearchItem>,
    deleted_paths: &[Box<str>],
) -> DeltaStats {
    let mut stats = DeltaStats::default();
    for path in deleted_paths {
        if let Some(index) = positions.remove(path.as_ref()) {
            let removed = items.swap_remove(index);
            stats.deleted += 1;
            stats.bytes_removed += item_memory_bytes(&removed);
            if let Some(moved) = items.get(index) {
                *positions
                    .get_mut(moved.path.as_ref())
                    .expect("indexed path") = index;
            }
        }
    }
    for item in upserts {
        stats.bytes_added += item_memory_bytes(&item);
        if let Some(&index) = positions.get(item.path.as_ref()) {
            stats.bytes_removed += item_memory_bytes(&items[index]);
            items[index] = item;
            stats.updated += 1;
        } else {
            positions.insert(item.path.clone(), items.len());
            items.push(item);
            stats.added += 1;
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SearchItemKind;

    fn item(path: &str, timestamp: i64) -> SearchItem {
        SearchItem {
            path: path.into(),
            modified_unix_secs: timestamp,
            kind: SearchItemKind::File,
        }
    }

    #[test]
    fn deletes_and_updates_keep_swapped_positions_and_memory_in_sync() {
        let mut items = vec![item("a", 1), item("b", 1), item("c", 1)];
        let mut positions = item_positions(&items);
        let before = crate::estimate_index_memory_bytes(&items);
        let stats = apply_delta(
            &mut items,
            &mut positions,
            vec![item("c", 2), item("new-long-name", 3)],
            &["a".into(), "a".into(), "missing".into()],
        );
        assert_eq!((stats.added, stats.updated, stats.deleted), (1, 1, 1));
        assert_eq!(
            before + stats.bytes_added - stats.bytes_removed,
            crate::estimate_index_memory_bytes(&items)
        );
        assert_eq!(items[positions["c"]].modified_unix_secs, 2);
        let stats = apply_delta(
            &mut items,
            &mut positions,
            vec![],
            &["c".into(), "b".into(), "new-long-name".into()],
        );
        assert_eq!(stats.deleted, 3);
        assert!(items.is_empty() && positions.is_empty());
    }

    #[test]
    fn small_delta_preserves_unaffected_path_allocations_in_large_corpus() {
        let mut items: Vec<_> = (0..100_000)
            .map(|n| item(&format!("file-{n}"), 0))
            .collect();
        let mut positions = item_positions(&items);
        let untouched = items[50_000].path.as_ptr();
        for batch in 0..100 {
            let stats = apply_delta(
                &mut items,
                &mut positions,
                vec![item("file-1", batch), item("extra", batch)],
                &[],
            );
            assert_eq!(stats.updated, if batch == 0 { 1 } else { 2 });
        }
        assert_eq!(items[positions["file-50000"]].path.as_ptr(), untouched);
        assert_eq!(items.len(), 100_001);
        assert_eq!(items[positions["extra"]].modified_unix_secs, 99);
    }
}
