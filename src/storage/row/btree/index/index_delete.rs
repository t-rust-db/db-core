// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Index b-tree delete (write path): entry delete plus underflow
//! handling. Mirrors `delete.rs` (table delete) in spirit but not in
//! mechanism — `insert.rs`'s module doc explains why: index b-tree
//! interior cells carry a full entry (its own value), not just a routing
//! key, so a delete target may be found sitting at interior level rather
//! than in a leaf, and removing a routing entry can never be conflated
//! with discarding whatever value that entry itself carries.
//!
//! **Underflow policy.** A page may be as sparse as it likes, but never
//! empty: SQLite refuses to open a non-root page with no cells (and an
//! interior root with none), so a tree holding one reads back as a
//! malformed file. When a delete empties a page, [`repair_emptied_page`]
//! takes it out of the tree and frees it. Its parent entry is a real index
//! entry, not just a routing key, so it is not dropped: it is put back
//! into the tree afterwards, where it sorts into the neighbouring subtree.
//! An interior page left with no entries (only its rightmost child) is
//! replaced by that child, and so on up to the root. This is a smaller
//! thing than SQLite's sibling-redistributing `balance()`: pages are not
//! merged while they are merely sparse. (Before this, emptied pages were
//! left in place, and emptying a large index produced a file sqlite3
//! called malformed.)
//!
//! **Interior-match deletion (predecessor swap).** When
//! [`super::descend_index_tree`] reports
//! [`IndexDescent::InteriorMatch`] (the target key IS an interior page's
//! own entry), the entry can't simply be dropped — its child pointer is
//! load-bearing (it routes to the subtree of lesser keys). The entry is
//! overwritten by its in-order predecessor, the last entry of the
//! rightmost leaf of that subtree ([`delete_interior_entry`]), which is
//! then removed from its leaf like any other leaf entry, so an emptied
//! leaf is repaired the same way.
//!
//! A tree already damaged by the old policy can hold empty pages on that
//! rightmost path; [`delete_via_predecessor_swap`] and
//! [`extract_max_entry`] are what a delete falls back to there. They find
//! the maximum of the subtree even when `rightmost` is drained, promoting
//! an interior page's own last entry; if the whole subtree is drained
//! the matched entry is removed outright instead of swapped.

use crate::storage::row::btree::index::insert::{insert_entry, write_interior_splitting};
use crate::storage::row::btree::index::{
    build_index_interior_cell, collect_index_interior_entries, collect_index_leaf_cells,
    decode_payload_len, descend_index_tree, search_index_leaf, write_index_interior_page,
    write_index_leaf_page, IndexDescent, IndexLeafCell, LeafSearch, INTERIOR_INDEX, LEAF_INDEX,
};
use crate::storage::row::btree::{
    local_payload_size, page1_header_start, read_num_cells, read_page_type, read_u32,
    splice_delete_cell, BtreeError, MAX_PAGES_VISITED,
};
use crate::storage::row::header::DatabaseHeader;
use crate::storage::row::pager::Pager;
use crate::storage::row::record::{TextEncoding, Value};

/// Returns the first overflow page of a value cell's raw bytes (`0` if
/// its payload is entirely local). `value_bytes` is the verbatim
/// `payload-length varint + local bytes [+ 4-byte overflow pointer]`
/// shape [`collect_index_leaf_cells`]/[`collect_index_interior_entries`]
/// already extract — decoding it directly (rather than re-reading from a
/// live page) works because that shape is self-contained, with the
/// payload-length varint at offset 0.
fn overflow_page_of(value_bytes: &[u8], usable_size: u32) -> Result<u32, BtreeError> {
    let (payload_len, tail_start) = decode_payload_len(value_bytes, 0, 0)?;
    let local_size = local_payload_size(usable_size, payload_len, true);
    if (local_size as u64) < payload_len {
        Ok(read_u32(
            value_bytes,
            tail_start.saturating_add(local_size),
            0,
        )?)
    } else {
        Ok(0)
    }
}

/// Walks and frees an overflow-page chain starting at `first_page` (a
/// no-op if it's `0` — the cell had no overflow). Mirrors
/// `table::delete::free_overflow_chain` (duplicated rather than shared —
/// see `insert.rs`'s `write_overflow_chain` doc comment for why).
fn free_overflow_chain(pager: &mut Pager, first_page: u32) -> Result<(), BtreeError> {
    let mut page_num = first_page;
    let mut visited = std::collections::HashSet::new();
    while page_num != 0 {
        if !visited.insert(page_num) {
            return Err(BtreeError::OverflowChainCycle {
                page_num: first_page,
                revisited_page: page_num,
            });
        }
        let next = {
            let buf = pager.get_page_mut(page_num)?;
            read_u32(buf, 0, page_num)?
        };
        pager.deallocate_page(page_num)?;
        page_num = next;
    }
    Ok(())
}

/// Deletes the entry with exactly `key` (via `compare_keys`) from the
/// index b-tree rooted at `root_page`. Returns `Err(BtreeError::KeyNotFound)`
/// if no such entry exists, leaving the tree unchanged. See the module
/// doc for how a target found at interior level (not just in a leaf) is
/// handled.
pub fn delete_entry(
    pager: &mut Pager,
    header: &DatabaseHeader,
    root_page: u32,
    key: &[Value],
    encoding: TextEncoding,
) -> Result<(), BtreeError> {
    let usable_size = header.usable_page_size();
    match descend_index_tree(pager, root_page, usable_size, key, encoding)? {
        IndexDescent::Leaf {
            ancestors,
            leaf_page,
        } => {
            delete_from_leaf(pager, usable_size, leaf_page, key, encoding)?;
            repair_if_empty(pager, header, root_page, encoding, &ancestors, leaf_page)
        }
        IndexDescent::InteriorMatch {
            ancestors,
            interior_page,
            entry_child,
        } => delete_interior_entry(
            pager,
            header,
            root_page,
            encoding,
            ancestors,
            interior_page,
            entry_child,
        ),
    }
}

/// Removes the cell matching `key` from `leaf_page` (an ordinary
/// leaf-level delete — `key` was found to genuinely live there). Writes
/// the leaf with whatever cells remain, even zero — see the module doc's
/// underflow policy.
fn delete_from_leaf(
    pager: &mut Pager,
    usable_size: u32,
    leaf_page: u32,
    key: &[Value],
    encoding: TextEncoding,
) -> Result<(), BtreeError> {
    let header_start = page1_header_start(leaf_page);
    let buf = pager.get_page_mut(leaf_page)?.clone();

    // Binary search for the exact match, decoding only the O(log n)
    // cells actually compared instead of every cell on the page (#648).
    let (pos, matched_cell) = match search_index_leaf(
        pager,
        &buf,
        header_start,
        leaf_page,
        usable_size,
        encoding,
        key,
    )? {
        LeafSearch::Found(pos, cell) => (pos, cell),
        LeafSearch::NotFound(_) => return Err(BtreeError::KeyNotFound),
    };
    let overflow_page = overflow_page_of(&matched_cell.1, usable_size)?;

    let buf = pager.get_page_mut(leaf_page)?;
    splice_delete_cell(buf, header_start, leaf_page, usable_size, pos, false)?;
    free_overflow_chain(pager, overflow_page)
}

/// Removes the entry that sits in `interior_page` as the separator in
/// front of `entry_child` (#722): it is overwritten by its in-order
/// predecessor, the last entry of the rightmost leaf below `entry_child`,
/// and the copy of that entry in its leaf is then removed (its overflow
/// chain now belongs to the interior cell, so it is not freed there; the
/// matched entry's own chain is freed instead). `ancestors` are the pages
/// above `interior_page`, root first. A predecessor larger than the entry
/// it replaces can make the interior page overflow, which splits it as an
/// insert would; so the predecessor's leaf is found again afterwards, by
/// descending for it, rather than trusting a path recorded before the
/// split. A leaf emptied by the removal is repaired.
fn delete_interior_entry(
    pager: &mut Pager,
    header: &DatabaseHeader,
    root_page: u32,
    encoding: TextEncoding,
    ancestors: Vec<u32>,
    interior_page: u32,
    entry_child: u32,
) -> Result<(), BtreeError> {
    let usable_size = header.usable_page_size();
    let Some((predecessor_key, predecessor_bytes)) =
        rightmost_leaf(pager, usable_size, encoding, entry_child)?
    else {
        // An empty leaf on the way down: the tree predates the repair.
        return delete_via_predecessor_swap(
            pager,
            header,
            root_page,
            encoding,
            &ancestors,
            interior_page,
            entry_child,
        );
    };

    let interior_start = page1_header_start(interior_page);
    let interior_buf = pager.get_page_mut(interior_page)?.clone();
    let (mut entries, rightmost) = collect_index_interior_entries(
        pager,
        &interior_buf,
        interior_start,
        interior_page,
        usable_size,
        encoding,
    )?;
    let entry = entries
        .iter_mut()
        .find(|(child, _, _)| *child == entry_child)
        .ok_or(BtreeError::Internal(
            "entry_child's routing entry must still exist in interior_page",
        ))?;
    let replaced_overflow = overflow_page_of(&entry.2, usable_size)?;
    entry.1 = predecessor_key.clone();
    entry.2 = predecessor_bytes;
    write_interior_splitting(
        pager,
        usable_size,
        interior_buf.len(),
        &ancestors,
        root_page,
        interior_page,
        entries,
        rightmost,
        encoding,
    )?;
    free_overflow_chain(pager, replaced_overflow)?;

    // The predecessor now sits in an interior page; its old copy is the
    // last entry of the rightmost leaf below that entry's left child.
    let IndexDescent::InteriorMatch {
        mut ancestors,
        interior_page,
        entry_child,
    } = descend_index_tree(pager, root_page, usable_size, &predecessor_key, encoding)?
    else {
        return Err(BtreeError::Internal(
            "the predecessor must be an interior entry after the swap",
        ));
    };
    ancestors.push(interior_page);
    let (path, leaf) = walk_rightmost(pager, entry_child, ancestors)?;
    let leaf_start = page1_header_start(leaf);
    let leaf_buf = pager.get_page_mut(leaf)?.clone();
    let cells =
        collect_index_leaf_cells(pager, &leaf_buf, leaf_start, leaf, usable_size, encoding)?;
    let matches_predecessor = cells.last().is_some_and(|(key, _)| {
        crate::storage::row::btree::index::compare_keys(key, &predecessor_key)
            == std::cmp::Ordering::Equal
    });
    if !matches_predecessor {
        return Err(BtreeError::Internal(
            "the predecessor's leaf copy must be the last entry of its leaf",
        ));
    }
    let last = cells.len().saturating_sub(1);
    let buf = pager.get_page_mut(leaf)?;
    splice_delete_cell(buf, leaf_start, leaf, usable_size, last, false)?;
    repair_if_empty(pager, header, root_page, encoding, &path, leaf)
}

/// Follows the rightmost child at every level from `page`, appending each
/// interior page passed to `path` (root first); returns `path` and the
/// leaf reached.
fn walk_rightmost(
    pager: &mut Pager,
    mut page: u32,
    mut path: Vec<u32>,
) -> Result<(Vec<u32>, u32), BtreeError> {
    let mut visited = 0usize;
    loop {
        visited = visited.saturating_add(1);
        if visited > MAX_PAGES_VISITED {
            return Err(BtreeError::TraversalTooLong {
                max: MAX_PAGES_VISITED,
            });
        }
        let buf = pager.get_page_mut(page)?.clone();
        let header_start = page1_header_start(page);
        let page_type = read_page_type(&buf, header_start, page)?;
        if page_type == LEAF_INDEX {
            return Ok((path, page));
        }
        if page_type != INTERIOR_INDEX {
            return Err(BtreeError::UnexpectedPageType {
                page_num: page,
                page_type,
            });
        }
        path.push(page);
        page = read_u32(&buf, header_start.saturating_add(8), page)?;
    }
}

/// The last entry (key and cell bytes) of the rightmost leaf below `page`,
/// or `None` if that leaf is empty.
fn rightmost_leaf(
    pager: &mut Pager,
    usable_size: u32,
    encoding: TextEncoding,
    page: u32,
) -> Result<Option<IndexLeafCell>, BtreeError> {
    let (_, leaf) = walk_rightmost(pager, page, Vec::new())?;
    let leaf_start = page1_header_start(leaf);
    let leaf_buf = pager.get_page_mut(leaf)?.clone();
    let cells =
        collect_index_leaf_cells(pager, &leaf_buf, leaf_start, leaf, usable_size, encoding)?;
    Ok(cells.last().cloned())
}

/// [`repair_emptied_page`] if `page` has no cells left.
fn repair_if_empty(
    pager: &mut Pager,
    header: &DatabaseHeader,
    root_page: u32,
    encoding: TextEncoding,
    ancestors: &[u32],
    page: u32,
) -> Result<(), BtreeError> {
    let buf = pager.get_page_mut(page)?.clone();
    if read_num_cells(&buf, page1_header_start(page), page)? > 0 {
        return Ok(());
    }
    repair_emptied_page(pager, header, root_page, encoding, ancestors, page)
}

/// Takes `page`, which has just lost its last cell, out of the tree it is
/// in (see the module doc's underflow policy). `ancestors` are the pages
/// above it, root first.
///
/// - The root left with no entries is a valid empty leaf, or, if it is an
///   interior page with only a rightmost child, becomes that child (the
///   root keeps its page number).
/// - A non-root interior page with no entries is its rightmost child: the
///   parent points there instead.
/// - An empty leaf is dropped from its parent. A parent entry whose left
///   child it was cannot just go, because it is a real index entry: it is
///   collected and put back into the tree at the end, and sorts into the
///   neighbouring subtree. If the leaf was the rightmost child, the last
///   entry's child takes its place the same way. A parent left with no
///   entries is then repaired in turn.
fn repair_emptied_page(
    pager: &mut Pager,
    header: &DatabaseHeader,
    root_page: u32,
    encoding: TextEncoding,
    ancestors: &[u32],
    page: u32,
) -> Result<(), BtreeError> {
    let usable_size = header.usable_page_size();
    let mut page = page;
    let mut ancestors = ancestors;
    let mut displaced: Vec<(Vec<Value>, Vec<u8>)> = Vec::new();
    loop {
        let header_start = page1_header_start(page);
        let buf = pager.get_page_mut(page)?.clone();
        let page_type = read_page_type(&buf, header_start, page)?;
        if read_num_cells(&buf, header_start, page)? > 0 {
            break;
        }
        let rightmost_of_page = if page_type == INTERIOR_INDEX {
            Some(read_u32(&buf, header_start.saturating_add(8), page)?)
        } else {
            None
        };
        let Some((&parent, upper)) = ancestors.split_last() else {
            // The root.
            let Some(child) = rightmost_of_page else {
                break; // an empty root leaf: an empty tree
            };
            // The child's cells are written into the root afresh rather
            // than its bytes copied over: the two pages need not lay
            // their header out at the same offset (page 1 does not).
            let child_start = page1_header_start(child);
            let child_buf = pager.get_page_mut(child)?.clone();
            match read_page_type(&child_buf, child_start, child)? {
                LEAF_INDEX => {
                    let cells = collect_index_leaf_cells(
                        pager,
                        &child_buf,
                        child_start,
                        child,
                        usable_size,
                        encoding,
                    )?;
                    let cells: Vec<Vec<u8>> = cells.into_iter().map(|(_, bytes)| bytes).collect();
                    let usable = pager.usable_size() as usize;
                    let buf = pager.get_page_mut(page)?;
                    write_index_leaf_page(buf, usable, header_start, page, &cells)?;
                }
                INTERIOR_INDEX => {
                    let (entries, rightmost) = collect_index_interior_entries(
                        pager,
                        &child_buf,
                        child_start,
                        child,
                        usable_size,
                        encoding,
                    )?;
                    write_interior(pager, page, header_start, &entries, rightmost)?;
                }
                page_type => {
                    return Err(BtreeError::UnexpectedPageType {
                        page_num: child,
                        page_type,
                    })
                }
            }
            pager.deallocate_page(child)?;
            continue;
        };

        let parent_start = page1_header_start(parent);
        let parent_buf = pager.get_page_mut(parent)?.clone();
        let (mut entries, mut parent_rightmost) = collect_index_interior_entries(
            pager,
            &parent_buf,
            parent_start,
            parent,
            usable_size,
            encoding,
        )?;
        if let Some(child) = rightmost_of_page {
            // An interior page that is only its rightmost child.
            if let Some(entry) = entries.iter_mut().find(|(c, _, _)| *c == page) {
                entry.0 = child;
            } else if parent_rightmost == page {
                parent_rightmost = child;
            } else {
                return Err(BtreeError::MissingChildRoute {
                    page_num: parent,
                    child: page,
                });
            }
            pager.deallocate_page(page)?;
            write_interior(pager, parent, parent_start, &entries, parent_rightmost)?;
            break;
        }
        // An empty leaf.
        if let Some(position) = entries.iter().position(|(c, _, _)| *c == page) {
            let (_, key, bytes) = entries.remove(position);
            displaced.push((key, bytes));
        } else if parent_rightmost == page {
            match entries.pop() {
                Some((child, key, bytes)) => {
                    parent_rightmost = child;
                    displaced.push((key, bytes));
                }
                None => {
                    // The parent's only child: the parent is empty as well.
                    pager.deallocate_page(page)?;
                    let usable = pager.usable_size() as usize;
                    let buf = pager.get_page_mut(parent)?;
                    write_index_leaf_page(buf, usable, parent_start, parent, &[])?;
                    page = parent;
                    ancestors = upper;
                    continue;
                }
            }
        } else {
            return Err(BtreeError::MissingChildRoute {
                page_num: parent,
                child: page,
            });
        }
        pager.deallocate_page(page)?;
        write_interior(pager, parent, parent_start, &entries, parent_rightmost)?;
        page = parent;
        ancestors = upper;
    }

    for (key, bytes) in displaced {
        insert_entry(pager, header, root_page, &key, encoding)?;
        free_overflow_chain(pager, overflow_page_of(&bytes, usable_size)?)?;
    }
    Ok(())
}

/// Rewrites interior page `page` as `entries` and `rightmost`.
fn write_interior(
    pager: &mut Pager,
    page: u32,
    header_start: usize,
    entries: &[crate::storage::row::btree::index::IndexInteriorEntry],
    rightmost: u32,
) -> Result<(), BtreeError> {
    let cells: Vec<Vec<u8>> = entries
        .iter()
        .map(|(child, _, value_bytes)| build_index_interior_cell(*child, value_bytes))
        .collect();
    let usable = pager.usable_size() as usize;
    let buf = pager.get_page_mut(page)?;
    write_index_interior_page(buf, usable, header_start, page, &cells, rightmost)
}

/// Handles a delete target found at interior level when the tree already
/// holds empty pages -- see the module doc for the predecessor-swap
/// algorithm this falls back to.
fn delete_via_predecessor_swap(
    pager: &mut Pager,
    header: &DatabaseHeader,
    root_page: u32,
    encoding: TextEncoding,
    ancestors: &[u32],
    interior_page: u32,
    entry_child: u32,
) -> Result<(), BtreeError> {
    let usable_size = header.usable_page_size();
    match extract_max_entry(pager, usable_size, entry_child, encoding, 0)? {
        Some(predecessor_bytes) => {
            let header_start = page1_header_start(interior_page);
            let buf = pager.get_page_mut(interior_page)?.clone();
            let (mut entries, rightmost) = collect_index_interior_entries(
                pager,
                &buf,
                header_start,
                interior_page,
                usable_size,
                encoding,
            )?;
            let entry = entries
                .iter_mut()
                .find(|(child, _, _)| *child == entry_child)
                .ok_or(BtreeError::Internal(
                    "entry_child's routing entry must still exist in interior_page",
                ))?;
            entry.2 = predecessor_bytes;
            let cell_bytes: Vec<Vec<u8>> = entries
                .iter()
                .map(|(child, _, value_bytes)| build_index_interior_cell(*child, value_bytes))
                .collect();
            let usable = pager.usable_size() as usize;
            let buf = pager.get_page_mut(interior_page)?;
            write_index_interior_page(
                buf,
                usable,
                header_start,
                interior_page,
                &cell_bytes,
                rightmost,
            )?;
        }
        None => {
            // `entry_child`'s entire subtree is drained (nothing left to
            // swap in) — the matched entry is deleted outright, since
            // there is nothing left to preserve.
            pager.deallocate_page(entry_child)?;
            remove_entry_by_child(pager, usable_size, interior_page, entry_child, encoding)?;
        }
    }
    // Dropping the separator may have left the interior page with no
    // entries: repair it as any emptied page is.
    repair_if_empty(pager, header, root_page, encoding, ancestors, interior_page)
}

/// Recursively finds and physically removes the maximum entry within the
/// subtree rooted at `page_num`, returning its raw cell bytes — or `None`
/// if that subtree holds no entries at all. See the module doc: this is
/// the one operation in this module that must actively restructure pages
/// (rather than just leaving emptied ones in place), because an
/// interior page's own last entry may need to become the new `rightmost`
/// once its previous `rightmost` subtree is confirmed drained.
///
/// Whenever this function determines that the subtree rooted at
/// `page_num` is itself fully drained (returning `Ok(None)`), it has
/// already deallocated every descendant page of `page_num` — never just
/// the immediate child — so a caller that sees `None` only needs to
/// deallocate `page_num` itself, not walk its subtree. `depth` guards
/// against a corrupted/cyclic `rightmost` chain causing unbounded
/// recursion, mirroring [`descend_index_tree`]'s `MAX_PAGES_VISITED`
/// convention.
fn extract_max_entry(
    pager: &mut Pager,
    usable_size: u32,
    page_num: u32,
    encoding: TextEncoding,
    depth: usize,
) -> Result<Option<Vec<u8>>, BtreeError> {
    if depth > MAX_PAGES_VISITED {
        return Err(BtreeError::TraversalTooLong {
            max: MAX_PAGES_VISITED,
        });
    }
    let header_start = page1_header_start(page_num);
    let buf = pager.get_page_mut(page_num)?.clone();
    let page_type = read_page_type(&buf, header_start, page_num)?;

    if page_type == LEAF_INDEX {
        let cells =
            collect_index_leaf_cells(pager, &buf, header_start, page_num, usable_size, encoding)?;
        let Some((_, max_bytes)) = cells.last().cloned() else {
            return Ok(None);
        };
        let last_idx = cells.len().saturating_sub(1);
        let buf = pager.get_page_mut(page_num)?;
        splice_delete_cell(buf, header_start, page_num, usable_size, last_idx, false)?;
        return Ok(Some(max_bytes));
    }
    if page_type != INTERIOR_INDEX {
        return Err(BtreeError::UnexpectedPageType {
            page_num,
            page_type,
        });
    }

    let (mut entries, rightmost) =
        collect_index_interior_entries(pager, &buf, header_start, page_num, usable_size, encoding)?;

    if let Some(max_bytes) = extract_max_entry(
        pager,
        usable_size,
        rightmost,
        encoding,
        depth.saturating_add(1),
    )? {
        // `rightmost`'s subtree had the maximum; it already rewrote
        // whichever page(s) it touched. This page's own shape (entries,
        // rightmost) is unaffected.
        return Ok(Some(max_bytes));
    }

    // `rightmost`'s subtree is fully drained — the recursive call above
    // has already deallocated every page in it. This page's own last
    // entry — if it has one — is the true maximum; its child becomes the
    // new `rightmost` so the (still possibly live) data under it stays
    // reachable.
    let Some((child, _, max_bytes)) = entries.pop() else {
        // This page's own entries are empty too, so `page_num` itself is
        // now fully drained. `rightmost` was already confirmed drained
        // above (and its own descendants already freed by the recursive
        // call) but never itself deallocated — do that now, since our
        // caller sees only `None` and has no other way to learn that
        // `rightmost` still needs freeing.
        pager.deallocate_page(rightmost)?;
        return Ok(None);
    };
    pager.deallocate_page(rightmost)?;
    let cell_bytes: Vec<Vec<u8>> = entries
        .iter()
        .map(|(c, _, value_bytes)| build_index_interior_cell(*c, value_bytes))
        .collect();
    let usable = pager.usable_size() as usize;
    let buf = pager.get_page_mut(page_num)?;
    write_index_interior_page(buf, usable, header_start, page_num, &cell_bytes, child)?;
    Ok(Some(max_bytes))
}

/// Removes the entry whose child is `child_to_remove` from `page_num`,
/// leaving `rightmost` and every other entry untouched (even if this
/// leaves `page_num` with zero entries — see the module doc's underflow
/// policy).
fn remove_entry_by_child(
    pager: &mut Pager,
    usable_size: u32,
    page_num: u32,
    child_to_remove: u32,
    encoding: TextEncoding,
) -> Result<(), BtreeError> {
    let header_start = page1_header_start(page_num);
    let buf = pager.get_page_mut(page_num)?.clone();
    let (mut entries, rightmost) =
        collect_index_interior_entries(pager, &buf, header_start, page_num, usable_size, encoding)?;
    let idx = entries
        .iter()
        .position(|(child, _, _)| *child == child_to_remove)
        .ok_or(BtreeError::MissingChildRoute {
            page_num,
            child: child_to_remove,
        })?;
    let (_, _, removed_value_bytes) = entries.remove(idx);
    let overflow_page = overflow_page_of(&removed_value_bytes, usable_size)?;
    let cell_bytes: Vec<Vec<u8>> = entries
        .iter()
        .map(|(c, _, value_bytes)| build_index_interior_cell(*c, value_bytes))
        .collect();
    let usable = pager.usable_size() as usize;
    let buf = pager.get_page_mut(page_num)?;
    write_index_interior_page(buf, usable, header_start, page_num, &cell_bytes, rightmost)?;
    free_overflow_chain(pager, overflow_page)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::storage::row::btree::index::insert::insert_entry;
    use crate::storage::row::btree::index::write_index_leaf_page;
    use crate::storage::row::vfs::{MemoryVfs, PageSource};
    use std::path::Path;

    fn minimal_index_db(page_size: u32) -> (MemoryVfs, DatabaseHeader) {
        let mut page1 = vec![0u8; page_size as usize];
        page1[0..16].copy_from_slice(b"SQLite format 3\0");
        page1[16..18].copy_from_slice(&(page_size as u16).to_be_bytes());
        page1[18] = 1;
        page1[19] = 1;
        page1[28..32].copy_from_slice(&1u32.to_be_bytes());
        page1[56..60].copy_from_slice(&1u32.to_be_bytes());
        write_index_leaf_page(&mut page1, 512, 100, 1, &[]).unwrap();

        let mut header_bytes = [0u8; 100];
        header_bytes.copy_from_slice(&page1[..100]);
        let header = DatabaseHeader::parse(&header_bytes).unwrap();

        let mut vfs = MemoryVfs::new();
        vfs.insert("/test.db", page1);
        (vfs, header)
    }

    fn key(a: &str, rowid: i64) -> Vec<Value> {
        vec![Value::Text(a.to_string().into()), Value::Integer(rowid)]
    }

    #[test]
    fn deleting_a_missing_key_errors() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();
        let err =
            delete_entry(&mut pager, &header, 1, &key("b", 2), TextEncoding::Utf8).unwrap_err();
        assert!(matches!(err, BtreeError::KeyNotFound));
    }

    #[test]
    fn deleting_the_only_entry_leaves_an_empty_root_leaf() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();
        delete_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();

        let header_start = page1_header_start(1);
        let buf = pager.get_page_mut(1).unwrap().clone();
        let page_type = read_page_type(&buf, header_start, 1).unwrap();
        assert_eq!(page_type, LEAF_INDEX);
        let cells = collect_index_leaf_cells(
            &pager,
            &buf,
            header_start,
            1,
            header.usable_page_size(),
            TextEncoding::Utf8,
        )
        .unwrap();
        assert!(cells.is_empty());
    }

    /// Regression test: index cells use a smaller `max_local` than table
    /// leaf cells (`(usable_size-12)*64/255-23`, not `usable_size-35` —
    /// 006-btree Requirement 7 flagged this via a real fixture), so a key
    /// well under the table threshold can still overflow here. Deleting
    /// such an entry must free its overflow chain, not leak it — mirrors
    /// `table::delete::tests::deleting_a_row_with_overflow_frees_its_overflow_chain`.
    #[test]
    fn deleting_an_entry_with_overflow_frees_its_overflow_chain() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        // `max_local` for a 512-byte page is (512-12)*64/255-23 = 102, so
        // 300 bytes of key text forces an overflow chain.
        let big_key = key(&"x".repeat(300), 1);
        insert_entry(&mut pager, &header, 1, &big_key, TextEncoding::Utf8).unwrap();

        let freelist_before = freelist_page_count(&mut pager);
        assert_eq!(freelist_before, 0);
        delete_entry(&mut pager, &header, 1, &big_key, TextEncoding::Utf8).unwrap();
        let freelist_after = freelist_page_count(&mut pager);

        assert!(
            freelist_after > freelist_before,
            "the entry's overflow chain must be returned to the freelist on delete"
        );
    }

    fn freelist_page_count(pager: &mut Pager) -> u32 {
        let page1 = pager.get_page_mut(1).unwrap().clone();
        u32::from_be_bytes(page1[36..40].try_into().unwrap())
    }

    /// The page count in page 1's header: every page the file has.
    fn file_page_count(pager: &mut Pager) -> u32 {
        let page1 = pager.get_page_mut(1).unwrap().clone();
        u32::from_be_bytes(page1[28..32].try_into().unwrap())
    }

    // ---- structure checks shared by the emptied-page tests ----

    /// Walks the tree under `page`, appending every entry's key in order,
    /// and asserts what SQLite insists on: no page but the root is empty,
    /// and an interior page has at least one entry.
    fn walk(
        pager: &mut Pager,
        header: &DatabaseHeader,
        page: u32,
        is_root: bool,
        out: &mut Vec<Vec<Value>>,
    ) {
        let usable = header.usable_page_size();
        let buf = pager.get_page_mut(page).unwrap().clone();
        let header_start = page1_header_start(page);
        match read_page_type(&buf, header_start, page).unwrap() {
            LEAF_INDEX => {
                let cells = collect_index_leaf_cells(
                    pager,
                    &buf,
                    header_start,
                    page,
                    usable,
                    TextEncoding::Utf8,
                )
                .unwrap();
                assert!(is_root || !cells.is_empty(), "leaf {page} is empty");
                out.extend(cells.into_iter().map(|(key, _)| key));
            }
            INTERIOR_INDEX => {
                let (entries, rightmost) = collect_index_interior_entries(
                    pager,
                    &buf,
                    header_start,
                    page,
                    usable,
                    TextEncoding::Utf8,
                )
                .unwrap();
                assert!(!entries.is_empty(), "interior page {page} has no entries");
                for (child, key, _) in entries {
                    walk(pager, header, child, false, out);
                    out.push(key);
                }
                walk(pager, header, rightmost, false, out);
            }
            other => panic!("page {page} has type {other:#x}"),
        }
    }

    /// The tree's keys in order, after checking it is well formed and its
    /// keys strictly ascend.
    fn keys_of(pager: &mut Pager, header: &DatabaseHeader, root: u32) -> Vec<Vec<Value>> {
        let mut keys = Vec::new();
        walk(pager, header, root, true, &mut keys);
        for pair in keys.windows(2) {
            assert_eq!(
                crate::storage::row::btree::index::compare_keys(&pair[0], &pair[1]),
                std::cmp::Ordering::Less,
                "keys out of order: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        keys
    }

    fn fixed_key(n: u32) -> Vec<Value> {
        key(&format!("key{n:06}"), i64::from(n))
    }

    fn insert_all_into(pager: &mut Pager, header: &DatabaseHeader, root: u32, count: u32) {
        for n in 0..count {
            insert_entry(pager, header, root, &fixed_key(n), TextEncoding::Utf8).unwrap();
        }
    }

    fn insert_all(pager: &mut Pager, header: &DatabaseHeader, count: u32) {
        for n in 0..count {
            insert_entry(pager, header, 1, &fixed_key(n), TextEncoding::Utf8).unwrap();
        }
    }

    /// 600 entries on 512-byte pages: three levels.
    const MANY: u32 = 600;

    fn tree_of_many() -> (MemoryVfs, DatabaseHeader) {
        minimal_index_db(512)
    }

    #[test]
    fn deleting_every_entry_in_ascending_order_leaves_a_valid_tree_throughout() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        insert_all(&mut pager, &header, MANY);
        assert_eq!(keys_of(&mut pager, &header, 1).len(), MANY as usize);
        for n in 0..MANY {
            delete_entry(&mut pager, &header, 1, &fixed_key(n), TextEncoding::Utf8).unwrap();
            if n % 7 == 0 {
                let expected: Vec<_> = (n + 1..MANY).map(fixed_key).collect();
                assert_eq!(keys_of(&mut pager, &header, 1), expected, "after {n}");
            }
        }
        assert!(keys_of(&mut pager, &header, 1).is_empty());
    }

    #[test]
    fn deleting_every_entry_in_descending_order_leaves_a_valid_tree_throughout() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        insert_all(&mut pager, &header, MANY);
        for n in (0..MANY).rev() {
            delete_entry(&mut pager, &header, 1, &fixed_key(n), TextEncoding::Utf8).unwrap();
            if n % 7 == 0 {
                let expected: Vec<_> = (0..n).map(fixed_key).collect();
                assert_eq!(keys_of(&mut pager, &header, 1), expected, "after {n}");
            }
        }
        assert!(keys_of(&mut pager, &header, 1).is_empty());
    }

    #[test]
    fn deleting_from_the_middle_outwards_leaves_a_valid_tree_throughout() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        insert_all(&mut pager, &header, MANY);
        let mut remaining: Vec<u32> = (0..MANY).collect();
        let mut step = 0;
        while !remaining.is_empty() {
            let at = remaining.len() / 2;
            let n = remaining.remove(at);
            delete_entry(&mut pager, &header, 1, &fixed_key(n), TextEncoding::Utf8).unwrap();
            step += 1;
            if step % 11 == 0 {
                let expected: Vec<_> = remaining.iter().copied().map(fixed_key).collect();
                assert_eq!(keys_of(&mut pager, &header, 1), expected, "step {step}");
            }
        }
        assert!(keys_of(&mut pager, &header, 1).is_empty());
    }

    #[test]
    fn an_emptied_tree_takes_entries_again() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        for round in 0..3 {
            insert_all(&mut pager, &header, MANY);
            for n in 0..MANY {
                delete_entry(&mut pager, &header, 1, &fixed_key(n), TextEncoding::Utf8).unwrap();
            }
            assert!(keys_of(&mut pager, &header, 1).is_empty(), "round {round}");
        }
        insert_all(&mut pager, &header, 10);
        assert_eq!(keys_of(&mut pager, &header, 1).len(), 10);
    }

    #[test]
    fn emptied_pages_go_back_to_the_freelist() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        insert_all(&mut pager, &header, MANY);
        let pages_in_use = file_page_count(&mut pager);
        for n in 0..MANY {
            delete_entry(&mut pager, &header, 1, &fixed_key(n), TextEncoding::Utf8).unwrap();
        }
        // Everything but the root is free again.
        assert_eq!(freelist_page_count(&mut pager), pages_in_use - 1);
    }

    /// Empties page `page` (a leaf) in place, as an older release left it.
    fn empty_leaf(pager: &mut Pager, page: u32) {
        let buf = pager.get_page_mut(page).unwrap();
        write_index_leaf_page(buf, 512, page1_header_start(page), page, &[]).unwrap();
    }

    /// The root's last separator entry: its key and the child to its left.
    fn last_separator(pager: &mut Pager, usable: u32, root: u32) -> (Vec<Value>, u32) {
        let buf = pager.get_page_mut(root).unwrap().clone();
        let (entries, _) = collect_index_interior_entries(
            pager,
            &buf,
            page1_header_start(root),
            root,
            usable,
            TextEncoding::Utf8,
        )
        .unwrap();
        let (child, key, _) = entries.last().cloned().unwrap();
        (key, child)
    }

    /// The rightmost child of interior page `page`.
    fn rightmost_child(pager: &mut Pager, page: u32) -> u32 {
        let buf = pager.get_page_mut(page).unwrap().clone();
        read_u32(&buf, page1_header_start(page).saturating_add(8), page).unwrap()
    }

    /// A tree already holding an empty leaf below a separator: the delete
    /// of that separator's key falls back to the predecessor swap and
    /// drops the emptied subtree outright.
    #[test]
    fn deleting_a_separator_over_an_emptied_leaf_drops_the_leaf() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        let root = crate::storage::row::btree::create_empty_index_root(&mut pager).unwrap();
        insert_all_into(&mut pager, &header, root, 40);
        let usable = header.usable_page_size();
        let (key, child) = last_separator(&mut pager, usable, root);
        empty_leaf(&mut pager, child);
        delete_entry(&mut pager, &header, root, &key, TextEncoding::Utf8).unwrap();
        // `keys_of` checks the tree is well formed: no empty leaf is left.
        let keys = keys_of(&mut pager, &header, root);
        assert!(!keys.contains(&key));
    }

    /// A three-level tree whose rightmost leaf under a separator has been
    /// emptied: the predecessor swap descends into an interior subtree and
    /// promotes the last entry of its own interior page.
    #[test]
    fn deleting_a_separator_over_an_interior_subtree_with_an_emptied_leaf() {
        let (vfs, header) = tree_of_many();
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
        let root = crate::storage::row::btree::create_empty_index_root(&mut pager).unwrap();
        insert_all_into(&mut pager, &header, root, MANY);
        let usable = header.usable_page_size();
        let (key, child) = last_separator(&mut pager, usable, root);
        // Walk down the child's rightmost path to its leaf.
        let mut page = child;
        loop {
            let buf = pager.get_page_mut(page).unwrap().clone();
            if read_page_type(&buf, page1_header_start(page), page).unwrap() == LEAF_INDEX {
                break;
            }
            page = rightmost_child(&mut pager, page);
        }
        empty_leaf(&mut pager, page);
        delete_entry(&mut pager, &header, root, &key, TextEncoding::Utf8).unwrap();
        let keys = keys_of(&mut pager, &header, root);
        assert!(!keys.contains(&key));
    }

    /// A small xorshift, so the random test is the same every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Random inserts and deletes (some keys large enough to overflow a
    /// 512-byte page) against a sorted model: the tree always holds
    /// exactly the model's keys, is always well formed, and never leaks a
    /// page.
    #[test]
    fn random_inserts_and_deletes_match_a_model_and_keep_the_tree_valid() {
        for seed in 1u64..=30 {
            let (vfs, header) = minimal_index_db(512);
            let mut pager = Pager::open(&vfs, Path::new("/test.db"), 512).unwrap();
            // A real index root: page 1's b-tree header sits after the
            // database header, so it holds less than any other page.
            let root = crate::storage::row::btree::create_empty_index_root(&mut pager).unwrap();
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut model: std::collections::BTreeMap<String, Vec<Value>> =
                std::collections::BTreeMap::new();
            let make = |n: u64, big: bool| {
                let text = if big {
                    format!("key{n:05}{}", "x".repeat(250))
                } else {
                    format!("key{n:05}")
                };
                (text.clone(), key(&text, n as i64))
            };
            for step in 0..2500u32 {
                let n = rng.next() % 400;
                let big = n.is_multiple_of(9);
                let (text, entry) = make(n, big);
                // Weighted towards inserts early and deletes late, so the
                // tree grows and then drains.
                let insert_bias = if step < 1200 { 70 } else { 30 };
                if rng.next() % 100 < insert_bias {
                    if let std::collections::btree_map::Entry::Vacant(slot) = model.entry(text) {
                        insert_entry(&mut pager, &header, root, &entry, TextEncoding::Utf8)
                            .unwrap();
                        slot.insert(entry);
                    }
                } else if model.remove(&text).is_some() {
                    delete_entry(&mut pager, &header, root, &entry, TextEncoding::Utf8).unwrap();
                } else {
                    let err = delete_entry(&mut pager, &header, root, &entry, TextEncoding::Utf8)
                        .unwrap_err();
                    assert!(matches!(err, BtreeError::KeyNotFound), "seed {seed}");
                }
                if step % 20 == 0 {
                    let expected: Vec<Vec<Value>> = model.values().cloned().collect();
                    assert_eq!(
                        keys_of(&mut pager, &header, root),
                        expected,
                        "seed {seed}, step {step}"
                    );
                }
            }
            for (_, entry) in std::mem::take(&mut model) {
                delete_entry(&mut pager, &header, root, &entry, TextEncoding::Utf8).unwrap();
            }
            assert!(keys_of(&mut pager, &header, root).is_empty(), "seed {seed}");
            // Nothing leaked: every page but page 1 and the root is free.
            assert_eq!(
                freelist_page_count(&mut pager),
                file_page_count(&mut pager) - 2,
                "seed {seed}: a page was leaked"
            );
        }
    }

    #[test]
    fn deleting_one_of_two_entries_keeps_the_other() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();
        insert_entry(&mut pager, &header, 1, &key("b", 2), TextEncoding::Utf8).unwrap();
        delete_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();

        let header_start = page1_header_start(1);
        let buf = pager.get_page_mut(1).unwrap().clone();
        let cells = collect_index_leaf_cells(
            &pager,
            &buf,
            header_start,
            1,
            header.usable_page_size(),
            TextEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].0, key("b", 2));
    }

    #[test]
    fn minimal_two_entry_split_then_delete_promoted_key() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        let filler = "x".repeat(190);
        let k0 = vec![
            Value::Text(format!("{filler}-0001").into()),
            Value::Integer(1),
        ];
        let k1 = vec![
            Value::Text(format!("{filler}-0002").into()),
            Value::Integer(2),
        ];
        insert_entry(&mut pager, &header, 1, &k0, TextEncoding::Utf8).unwrap();
        insert_entry(&mut pager, &header, 1, &k1, TextEncoding::Utf8).unwrap();
        delete_entry(&mut pager, &header, 1, &k1, TextEncoding::Utf8).unwrap();
    }

    #[test]
    fn split_then_delete_all_including_promoted_interior_entries() {
        // Small pages + ~200-byte keys force splits (and therefore
        // entries promoted to interior level) well before 30 entries —
        // regression guard for the interior-match predecessor swap:
        // deleting every entry, in ascending order, must never error and
        // must leave the tree fully empty (per the read-side cursor).
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        let filler = "x".repeat(190);
        let n = 30i64;
        let keys: Vec<Vec<Value>> = (1..=n)
            .map(|i| {
                vec![
                    Value::Text(format!("{filler}-{i:04}").into()),
                    Value::Integer(i),
                ]
            })
            .collect();
        for k in &keys {
            insert_entry(&mut pager, &header, 1, k, TextEncoding::Utf8).unwrap();
        }
        for (idx, k) in keys.iter().enumerate() {
            let r = delete_entry(&mut pager, &header, 1, k, TextEncoding::Utf8);
            if let Err(e) = r {
                panic!("delete failed at idx {idx}: {:?}", e);
            }
        }

        let mut cursor =
            crate::storage::row::btree::IndexCursor::new(pager, header.usable_page_size(), 1);
        assert!(cursor.first().unwrap().is_none());
    }

    #[test]
    fn split_then_delete_descending_hits_the_interior_predecessor_swap_with_a_nonempty_subtree() {
        // `split_then_delete_all_including_promoted_interior_entries`
        // deletes ascending, which drains each promoted key's left
        // subtree to empty before that key is ever the delete target --
        // always the `None` (subtree exhausted) arm of
        // `delete_via_predecessor_swap`. Deleting descending instead
        // reaches a promoted interior key while its left subtree still
        // has entries left to extract a predecessor from -- the `Some`
        // arm, which rewrites the interior page with the swapped-in key.
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        let filler = "x".repeat(190);
        let n = 30i64;
        let keys: Vec<Vec<Value>> = (1..=n)
            .map(|i| {
                vec![
                    Value::Text(format!("{filler}-{i:04}").into()),
                    Value::Integer(i),
                ]
            })
            .collect();
        for k in &keys {
            insert_entry(&mut pager, &header, 1, k, TextEncoding::Utf8).unwrap();
        }
        for (idx, k) in keys.iter().enumerate().rev() {
            let r = delete_entry(&mut pager, &header, 1, k, TextEncoding::Utf8);
            if let Err(e) = r {
                panic!("delete failed at idx {idx}: {:?}", e);
            }
        }

        let mut cursor =
            crate::storage::row::btree::IndexCursor::new(pager, header.usable_page_size(), 1);
        assert!(cursor.first().unwrap().is_none());
    }

    /// Walks every page number reachable from `page_num` (an index b-tree
    /// root/subtree): itself, plus (for an interior page) every entry's
    /// child and `rightmost`, recursively.
    fn reachable_pages(pager: &Pager, page_num: u32, encoding: TextEncoding, out: &mut Vec<u32>) {
        out.push(page_num);
        let header_start = page1_header_start(page_num);
        let buf = pager.read_page(page_num).unwrap();
        let page_type = read_page_type(&buf, header_start, page_num).unwrap();
        if page_type != INTERIOR_INDEX {
            return;
        }
        let (entries, rightmost) = collect_index_interior_entries(
            pager,
            &buf,
            header_start,
            page_num,
            buf.len() as u32,
            encoding,
        )
        .unwrap();
        for (child, _, _) in &entries {
            reachable_pages(pager, *child, encoding, out);
        }
        reachable_pages(pager, rightmost, encoding, out);
    }

    /// Walks the freelist trunk chain (page 1's header fields), returning
    /// every page number currently on it (trunks and leaves alike).
    fn freelist_pages(pager: &Pager) -> Vec<u32> {
        let page1 = pager.read_page(1).unwrap();
        let mut trunk = u32::from_be_bytes([page1[32], page1[33], page1[34], page1[35]]);
        let mut out = Vec::new();
        while trunk != 0 {
            out.push(trunk);
            let buf = pager.read_page(trunk).unwrap();
            let parsed = crate::storage::row::pager::freelist::TrunkPage::parse(&buf).unwrap();
            out.extend(&parsed.leaves);
            trunk = parsed.next_trunk;
        }
        out
    }

    #[test]
    fn deleting_all_entries_orphans_no_page() {
        // Regression guard for the `extract_max_entry` fix: a predecessor
        // swap that drains a subtree more than one level deep (interior ->
        // interior -> leaf) must deallocate every page in that subtree,
        // not just the immediate child — otherwise a deeper emptied page
        // becomes unreachable garbage (neither reachable from the root
        // nor on the freelist) instead of landing on the freelist. Every
        // page in the file must be either reachable from the root or on
        // the freelist — nothing left in neither set.
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        let filler = "x".repeat(190);
        let n = 30i64;
        let keys: Vec<Vec<Value>> = (1..=n)
            .map(|i| {
                vec![
                    Value::Text(format!("{filler}-{i:04}").into()),
                    Value::Integer(i),
                ]
            })
            .collect();
        for k in &keys {
            insert_entry(&mut pager, &header, 1, k, TextEncoding::Utf8).unwrap();
        }
        for k in &keys {
            delete_entry(&mut pager, &header, 1, k, TextEncoding::Utf8).unwrap();
        }

        let raw = pager.get_page_mut(1).unwrap().clone();
        let total_pages = u32::from_be_bytes([raw[28], raw[29], raw[30], raw[31]]);

        let mut reachable = Vec::new();
        reachable_pages(&pager, 1, TextEncoding::Utf8, &mut reachable);
        let freed = freelist_pages(&pager);

        let mut accounted: Vec<u32> = reachable.into_iter().chain(freed).collect();
        accounted.sort_unstable();
        accounted.dedup();
        let all_pages: Vec<u32> = (1..=total_pages).collect();
        assert_eq!(
            accounted, all_pages,
            "every page must be either reachable from the root or on the freelist — \
             a page missing from both is orphaned"
        );
    }

    /// Kills the `depth > MAX_PAGES_VISITED` -> `==`/`>=` mutants: at
    /// exactly `MAX_PAGES_VISITED`, the real `>` guard has NOT yet
    /// tripped (only depths strictly greater than the max are rejected),
    /// so a call at that exact depth must still succeed.
    #[test]
    fn extract_max_entry_does_not_reject_depth_exactly_at_the_limit() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();

        let result = extract_max_entry(
            &mut pager,
            header.usable_page_size(),
            1,
            TextEncoding::Utf8,
            MAX_PAGES_VISITED,
        )
        .unwrap();
        assert!(
            result.is_some(),
            "depth == MAX_PAGES_VISITED must still be processed, not rejected"
        );
    }

    /// The mirror case: one past the limit must be rejected.
    #[test]
    fn extract_max_entry_rejects_depth_past_the_limit() {
        let page_size = 512u32;
        let (vfs, header) = minimal_index_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_entry(&mut pager, &header, 1, &key("a", 1), TextEncoding::Utf8).unwrap();

        let err = extract_max_entry(
            &mut pager,
            header.usable_page_size(),
            1,
            TextEncoding::Utf8,
            MAX_PAGES_VISITED + 1,
        )
        .unwrap_err();
        assert!(matches!(err, BtreeError::TraversalTooLong { .. }));
    }
}
