// Copyright 2026 Schuberg Philis
// SPDX-License-Identifier: Apache-2.0
//! Table b-tree update (write path): rewrite an existing row's payload
//! under its unchanged rowid without the redundant double root-to-leaf
//! descent `delete_row` followed by `insert_row` costs (db-core#524).
//! `Opcode::Update`'s codegen (`UPDATE ... WHERE`, rowid unchanged) uses
//! this instead of the `Delete`+`Insert` opcode pair when the row's
//! rowid isn't being reassigned.

use super::delete::free_overflow_chain;
use super::insert::{encode_leaf_cell, insert_into_leaf};
use crate::storage::row::btree::{
    find_leaf_cell, find_leaf_page, page1_header_start, splice_delete_cell, BtreeError,
};
use crate::storage::row::header::DatabaseHeader;
use crate::storage::row::pager::Pager;

/// Rewrites the row with `rowid` in the table b-tree rooted at
/// `root_page` to hold `new_payload`, doing a single `find_leaf_page`
/// descent shared between the removal of the old cell and the insertion
/// of the new one (as opposed to `delete_row(...)` followed by
/// `insert_row(...)`, which each independently walk root-to-leaf).
/// Returns `Err(BtreeError::RowidNotFound)` if no such row exists.
///
/// Falls back to the two-descent `delete_row`+`insert_row` path only
/// when the old cell is its leaf's last row (deleting it would collapse
/// the page into its ancestors, invalidating `leaf_page` for the
/// immediate reinsertion this function otherwise does) -- rare for the
/// `UPDATE ... WHERE` shape this optimizes (many rows on a shared leaf).
pub fn update_row(
    pager: &mut Pager,
    header: &DatabaseHeader,
    root_page: u32,
    rowid: i64,
    new_payload: &[u8],
) -> Result<(), BtreeError> {
    let usable_size = header.usable_page_size();
    let (ancestors, leaf_page) = find_leaf_page(pager, root_page, rowid)?;
    let header_start = page1_header_start(leaf_page);

    let buf = pager.get_page_mut(leaf_page)?;
    let (pos, num_cells, overflow_page) =
        find_leaf_cell(buf, header_start, leaf_page, usable_size, rowid)?
            .ok_or(BtreeError::RowidNotFound { rowid })?;

    if num_cells <= 1 && !ancestors.is_empty() {
        // Deleting this cell would empty and collapse the leaf -- fall
        // back to the proven two-descent path rather than reinserting
        // into a page that may no longer exist.
        super::delete::delete_row(pager, header, root_page, rowid)?;
        return super::insert::insert_row(pager, header, root_page, rowid, new_payload);
    }

    splice_delete_cell(buf, header_start, leaf_page, usable_size, pos, true)?;
    free_overflow_chain(pager, overflow_page)?;

    let cell = encode_leaf_cell(pager, usable_size, rowid, new_payload)?;
    let page_len = pager.get_page_mut(leaf_page)?.len();
    insert_into_leaf(
        pager,
        usable_size,
        page_len,
        leaf_page,
        root_page,
        &ancestors,
        rowid,
        cell,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::storage::row::btree::insert_row;
    use crate::storage::row::btree::test_minimal_db as minimal_db;
    use std::path::Path;

    #[test]
    fn updating_a_missing_rowid_errors() {
        let page_size = 512u32;
        let (vfs, header) = minimal_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_row(&mut pager, &header, 1, 1, b"hello").unwrap();
        let err = update_row(&mut pager, &header, 1, 2, b"world").unwrap_err();
        assert!(matches!(err, BtreeError::RowidNotFound { rowid: 2 }));
    }

    /// #524 tagged MC/DC vector (obligation
    /// `storage_row_btree_table_table_update_update_row_64d9ae82`,
    /// decision `num_cells <= 1 && !ancestors.is_empty()`): leaf A
    /// (`num_cells <= 1`) false short-circuits the decision to false --
    /// a leaf with other rows left takes the in-place splice-and-reinsert
    /// path regardless of `ancestors`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_row_btree_table_table_update_update_row_64d9ae82__v1_other_rows_remain_on_the_leaf(
    ) {
        let page_size = 512u32;
        let (vfs, header) = minimal_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_row(&mut pager, &header, 1, 1, b"hello").unwrap();
        insert_row(&mut pager, &header, 1, 2, b"world").unwrap();
        insert_row(&mut pager, &header, 1, 3, b"third").unwrap();

        update_row(&mut pager, &header, 1, 2, b"WORLD!").unwrap();

        let header_start = page1_header_start(1);
        let buf = pager.get_page_mut(1).unwrap().clone();
        let cells = crate::storage::row::btree::collect_leaf_cells(
            &buf,
            header_start,
            1,
            header.usable_page_size(),
        )
        .unwrap();
        assert_eq!(cells.len(), 3, "row count must be unchanged by an update");
        let updated = cells.iter().find(|(r, _)| *r == 2).unwrap();
        assert!(
            updated.1.ends_with(b"WORLD!"),
            "updated cell should carry the new payload: {:?}",
            updated.1
        );
    }

    /// #524 tagged MC/DC vector (obligation
    /// `storage_row_btree_table_table_update_update_row_64d9ae82`): leaf
    /// A true, leaf B (`ancestors.is_empty()`) true (so `!ancestors.is_empty()`
    /// is false) independently flips the outcome to false -- the
    /// single-page root-leaf case, where the (empty) root can never be
    /// collapsed away, so the in-place path is safe even with one cell.
    /// Independence pair for B against
    /// `mcdc__storage_row_btree_table_table_update_update_row_64d9ae82__v3_sole_row_of_a_leaf_with_ancestors`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_row_btree_table_table_update_update_row_64d9ae82__v2_only_row_in_root_leaf_has_no_ancestors(
    ) {
        // num_cells == 1 with no ancestors -- takes the in-place path
        // (deleting the sole cell on the root leaf doesn't collapse it).
        let page_size = 512u32;
        let (vfs, header) = minimal_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_row(&mut pager, &header, 1, 1, b"hello").unwrap();
        update_row(&mut pager, &header, 1, 1, b"goodbye").unwrap();

        let header_start = page1_header_start(1);
        let buf = pager.get_page_mut(1).unwrap().clone();
        let cells = crate::storage::row::btree::collect_leaf_cells(
            &buf,
            header_start,
            1,
            header.usable_page_size(),
        )
        .unwrap();
        assert_eq!(cells.len(), 1);
        assert!(cells[0].1.ends_with(b"goodbye"));
    }

    /// #524 tagged MC/DC vector (obligation
    /// `storage_row_btree_table_table_update_update_row_64d9ae82`): both
    /// leafs true -- a multi-page tree drained down to one leaf holding
    /// exactly one cell, so updating it takes the two-descent
    /// `delete_row`+`insert_row` fallback rather than reinserting into a
    /// page `splice_delete_cell` would otherwise collapse. Independence
    /// pair for A against
    /// `mcdc__storage_row_btree_table_table_update_update_row_64d9ae82__v1_other_rows_remain_on_the_leaf`.
    #[test]
    #[allow(non_snake_case)]
    fn mcdc__storage_row_btree_table_table_update_update_row_64d9ae82__v3_sole_row_of_a_leaf_with_ancestors(
    ) {
        let page_size = 512u32;
        let (vfs, header) = minimal_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        let filler = "x".repeat(190);
        let n = 60i64;
        for i in 1..=n {
            insert_row(
                &mut pager,
                &header,
                1,
                i,
                format!("{filler}-{i:04}").as_bytes(),
            )
            .unwrap();
        }
        for i in 1..n {
            crate::storage::row::btree::delete_row(&mut pager, &header, 1, i).unwrap();
        }
        // Rowid `n` is now the sole cell of its leaf, with non-empty
        // ancestors (the tree is still multi-page).
        update_row(&mut pager, &header, 1, n, b"replacement").unwrap();

        let (_, leaf_page) = find_leaf_page(&mut pager, 1, n).unwrap();
        let header_start = page1_header_start(leaf_page);
        let buf = pager.get_page_mut(leaf_page).unwrap().clone();
        let cells = crate::storage::row::btree::collect_leaf_cells(
            &buf,
            header_start,
            leaf_page,
            header.usable_page_size(),
        )
        .unwrap();
        assert_eq!(cells.len(), 1);
        assert!(cells[0].1.ends_with(b"replacement"));
    }

    #[test]
    fn updating_to_a_much_larger_payload_still_finds_the_row() {
        // Exercises the fallback-to-split path inside `insert_into_leaf`
        // when the rewritten row no longer fits in the freed slot.
        let page_size = 512u32;
        let (vfs, header) = minimal_db(page_size);
        let mut pager = Pager::open(&vfs, Path::new("/test.db"), page_size).unwrap();

        insert_row(&mut pager, &header, 1, 1, b"hello").unwrap();
        insert_row(&mut pager, &header, 1, 2, b"world").unwrap();

        let big = "x".repeat(300);
        update_row(&mut pager, &header, 1, 1, big.as_bytes()).unwrap();

        let (_, leaf_page) = find_leaf_page(&mut pager, 1, 1).unwrap();
        let header_start = page1_header_start(leaf_page);
        let buf = pager.get_page_mut(leaf_page).unwrap().clone();
        let cells = crate::storage::row::btree::collect_leaf_cells(
            &buf,
            header_start,
            leaf_page,
            header.usable_page_size(),
        )
        .unwrap();
        let updated = cells.iter().find(|(r, _)| *r == 1).unwrap();
        assert!(updated.1.ends_with(big.as_bytes()));
    }
}
