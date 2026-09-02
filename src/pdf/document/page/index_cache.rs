use crate::bindgen::{FPDF_DOCUMENT, FPDF_PAGE};
use crate::pdf::document::page::PdfPageContentRegenerationStrategy;
use crate::pdf::document::pages::PdfPageIndex;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A cache of [PdfPageIndex] indices for all open [PdfPage] objects.
/// We keep track of these so that we can return accurate [PdfPageIndex] values to
/// the object copying functions in [PdfPageObjectGroup], some of which depend upon
/// accurate source page indices.
static PAGE_INDEX_CACHE: Lazy<Mutex<PdfPageIndexCache>> =
    Lazy::new(|| Mutex::new(PdfPageIndexCache::new()));

struct PdfPageCachedProperties {
    index: PdfPageIndex,
    content_regeneration_strategy: PdfPageContentRegenerationStrategy,
}

pub(crate) struct PdfPageIndexCache {
    pages_by_index: HashMap<(FPDF_DOCUMENT, FPDF_PAGE), PdfPageCachedProperties>,
    indices_by_page: HashMap<(FPDF_DOCUMENT, PdfPageIndex), FPDF_PAGE>,
    documents_by_maximum_index: HashMap<FPDF_DOCUMENT, PdfPageIndex>,
}

impl PdfPageIndexCache {
    #[inline]
    fn new() -> Self {
        Self {
            pages_by_index: HashMap::new(),
            indices_by_page: HashMap::new(),
            documents_by_maximum_index: HashMap::new(),
        }
    }

    /// Returns the currently cached properties for the given raw document and page handles, if any.
    #[inline]
    fn get(&self, document: FPDF_DOCUMENT, page: FPDF_PAGE) -> Option<&PdfPageCachedProperties> {
        self.pages_by_index.get(&(document, page))
    }

    /// Returns the number of `pages_by_index` entries currently cached for the given raw document
    /// handle. The [PAGE_INDEX_CACHE] is process-global and shared by every open document, so
    /// counting entries scoped to a single document is the only way a test can assert on cache
    /// contents without depending on what other, concurrently running tests happen to have cached.
    #[cfg(test)]
    #[inline]
    fn count_for_document(&self, document: FPDF_DOCUMENT) -> usize {
        self.pages_by_index
            .keys()
            .filter(|(cached_document, _)| *cached_document == document)
            .count()
    }

    /// Sets the currently cached properties for the given raw document and page handles.
    #[inline]
    fn set(&mut self, document: FPDF_DOCUMENT, page: FPDF_PAGE, props: PdfPageCachedProperties) {
        // Keep track of the maximum page index for this document. We'll need to know this
        // if we have to shuffle indices to accommodate page insertions or deletions.

        match self.documents_by_maximum_index.get(&document).copied() {
            Some(maximum) => {
                if props.index > maximum {
                    self.documents_by_maximum_index
                        .insert(document, props.index);
                }
            }
            None => {
                self.documents_by_maximum_index
                    .insert(document, props.index);
            }
        }

        self.indices_by_page.insert((document, props.index), page);
        self.pages_by_index.insert((document, page), props);
    }

    /// Removes the cached [PdfPageIndex] value for the given raw document and page handles.
    #[inline]
    fn remove(
        &mut self,
        document: FPDF_DOCUMENT,
        page: FPDF_PAGE,
    ) -> Option<PdfPageCachedProperties> {
        let props = self.pages_by_index.remove(&(document, page));

        if let Some(props) = props.as_ref() {
            self.indices_by_page.remove(&(document, props.index));

            if self.documents_by_maximum_index.get(&document).copied() == Some(props.index) {
                // This page had the maximum page index for this document. Now that it's been removed
                // from the cache, we need to find the new maximum page index for this document.
                //
                // The search has to be scoped to this document. `indices_by_page` is shared by
                // every open document, so asking whether it is globally empty answers the wrong
                // question: whenever some other document still holds entries, this document falls
                // through to the search below and, having no entries of its own left, records a
                // maximum index of zero instead of dropping out of the map. That stale entry
                // outlives the document, and pdfium reuses `FPDF_DOCUMENT` addresses, so the next
                // document allocated at the same address inherits a maximum index it never set,
                // corrupting the index shuffling in `insert()` and `delete()`.

                let maximum = self
                    .indices_by_page
                    .keys()
                    .filter(|(cached_document, _)| *cached_document == document)
                    .map(|(_, index)| *index)
                    .max();

                match maximum {
                    Some(maximum) => {
                        self.documents_by_maximum_index.insert(document, maximum);
                    }
                    None => {
                        // There's no longer any page indices cached for this document.

                        self.documents_by_maximum_index.remove(&document);
                    }
                }
            }
        }

        props
    }

    /// Adjusts all cached [PdfPageIndex] values for the given document as necessary to accommodate
    /// an insertion of the given number of pages at the given index position.
    #[inline]
    fn insert(&mut self, document: FPDF_DOCUMENT, index: PdfPageIndex, count: PdfPageIndex) {
        match self.documents_by_maximum_index.get(&document).copied() {
            Some(maximum_index_for_document) => {
                if maximum_index_for_document > index {
                    // Shuffle down all page indices in the document after the given index position.

                    for index in (index..=maximum_index_for_document).rev() {
                        if let Some(page) = self.indices_by_page.get(&(document, index)).copied() {
                            // Update the indices of this page.

                            let props = self.remove(document, page);

                            let content_regeneration_strategy = if let Some(props) = props {
                                props.content_regeneration_strategy
                            } else {
                                PdfPageContentRegenerationStrategy::AutomaticOnEveryChange
                            };

                            self.set(
                                document,
                                page,
                                PdfPageCachedProperties {
                                    index: index + count,
                                    content_regeneration_strategy,
                                },
                            );
                        }
                    }
                }

                self.documents_by_maximum_index
                    .insert(document, maximum_index_for_document + count);
            }
            None => {
                // This is the first page index we're caching for this document.

                self.documents_by_maximum_index
                    .insert(document, index + count - 1);
            }
        }
    }

    /// Adjusts all cached [PdfPageIndex] values for the given document as necessary to accommodate
    /// a deletion of the given number of pages at the given index position.
    #[inline]
    fn delete(&mut self, document: FPDF_DOCUMENT, index: PdfPageIndex, count: PdfPageIndex) {
        // Shuffle up all page indices in the document after the given index position.

        let mut maximum_index_for_document = self
            .documents_by_maximum_index
            .get(&document)
            .copied()
            .unwrap_or(0);

        // Remove the deleted pages from the cache.

        for index in index..index + count {
            if let Some(page) = self.indices_by_page.get(&(document, index)).copied() {
                self.remove(document, page);
            }
        }

        if maximum_index_for_document > index {
            // Shuffle up all page indices in the document after the given index position.

            for index in index + 1..=maximum_index_for_document {
                if let Some(page) = self.indices_by_page.get(&(document, index)).copied() {
                    // Update the indices of this page.

                    let props = self.remove(document, page);

                    let content_regeneration_strategy = if let Some(props) = props {
                        props.content_regeneration_strategy
                    } else {
                        PdfPageContentRegenerationStrategy::AutomaticOnEveryChange
                    };

                    self.set(
                        document,
                        page,
                        PdfPageCachedProperties {
                            index: index - count,
                            content_regeneration_strategy,
                        },
                    );
                }
            }
        } else {
            maximum_index_for_document = index;
        }

        // Update the maximum index position for this document.

        if maximum_index_for_document >= count {
            self.documents_by_maximum_index
                .insert(document, maximum_index_for_document - count);
        } else {
            // There's no longer any page indices cached for this document.

            self.documents_by_maximum_index.remove(&document);
        }
    }

    /// Locks the process-global [PAGE_INDEX_CACHE], recovering the guard if the mutex has been
    /// poisoned by a panic in another thread.
    ///
    /// Recovering here is deliberate. The cache holds nothing but `Copy` handles and page indices
    /// in three `HashMap`s, so a panic while the guard was held cannot leave it in an unsound
    /// state; the worst case is a stale index entry for a page that is going away anyway.
    ///
    /// Unwrapping instead escalates any single panic under the lock into a process abort.
    /// [PdfPage::drop_impl] takes this same lock, so once the mutex is poisoned every subsequent
    /// page drop panics inside a destructor, and Rust turns a panic during unwinding into a
    /// non-unwinding abort (SIGABRT). One failed assertion anywhere in the crate then kills the
    /// whole process instead of failing the one operation that went wrong.
    #[inline]
    fn lock() -> MutexGuard<'static, PdfPageIndexCache> {
        PAGE_INDEX_CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    // The remaining methods in this implementation take care of thread-safe locking.
    // These methods form the public API of the cache.

    /// Caches the given properties for the given raw document and page handles.
    #[inline]
    pub(crate) fn cache_props_for_page(
        document: FPDF_DOCUMENT,
        page: FPDF_PAGE,
        index: PdfPageIndex,
        content_regeneration_strategy: PdfPageContentRegenerationStrategy,
    ) {
        Self::lock().set(
            document,
            page,
            PdfPageCachedProperties {
                index,
                content_regeneration_strategy,
            },
        )
    }

    /// Returns the current [PdfPageIndex] value for the given raw document and page handles, if any.
    #[inline]
    pub(crate) fn get_index_for_page(
        document: FPDF_DOCUMENT,
        page: FPDF_PAGE,
    ) -> Option<PdfPageIndex> {
        Self::lock().get(document, page).map(|props| props.index)
    }

    /// Returns the current [PdfPageContentRegenerationStrategy] value for the given raw document
    /// and page handles, if any.
    #[inline]
    pub(crate) fn get_content_regeneration_strategy_for_page(
        document: FPDF_DOCUMENT,
        page: FPDF_PAGE,
    ) -> Option<PdfPageContentRegenerationStrategy> {
        Self::lock()
            .get(document, page)
            .map(|props| props.content_regeneration_strategy)
    }

    /// Removes the cached [PdfPageIndex] value for the given raw document and page handles.
    #[inline]
    pub(crate) fn remove_index_for_page(document: FPDF_DOCUMENT, page: FPDF_PAGE) {
        Self::lock().remove(document, page);
    }

    /// Adjusts all cached [PdfPageIndex] values for the given document as necessary to accommodate
    /// an insertion of the given number of pages at the given index position.
    #[inline]
    pub(crate) fn insert_pages_at_index(
        document: FPDF_DOCUMENT,
        index: PdfPageIndex,
        count: PdfPageIndex,
    ) {
        Self::lock().insert(document, index, count);
    }

    /// Adjusts all cached [PdfPageIndex] values for the given document as necessary to accommodate
    /// a deletion of the given number of pages at the given index position.
    #[inline]
    pub(crate) fn delete_pages_at_index(
        document: FPDF_DOCUMENT,
        index: PdfPageIndex,
        count: PdfPageIndex,
    ) {
        Self::lock().delete(document, index, count);
    }
}

unsafe impl Send for PdfPageIndexCache {}

unsafe impl Sync for PdfPageIndexCache {}

#[cfg(test)]
mod tests {
    use crate::pdf::document::page::index_cache::{PdfPageIndexCache, PAGE_INDEX_CACHE};
    use crate::prelude::*;
    use crate::utils::test::test_bind_to_pdfium;

    #[test]
    fn test_cache_instantiation() -> Result<(), PdfiumError> {
        let pdfium = test_bind_to_pdfium();

        let mut document = pdfium.create_new_pdf()?;

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document.handle()),
            0
        );

        {
            // Now let's create a blank page and get a handle to it...

            let _page = document
                .pages_mut()
                .create_page_at_start(PdfPagePaperSize::a4())?;

            // ... and confirm the cache updated.

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                1
            );
        }

        // The page has dropped out of scope. Confirm the cache got cleaned up.

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document.handle()),
            0
        );

        // Get a new handle to the page...

        let _page = document.pages().first();

        // ... and confirm the cache updated.

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document.handle()),
            1
        );

        Ok(())
    }

    #[test]
    fn test_get_and_set_index_for_page() -> Result<(), PdfiumError> {
        let pdfium = test_bind_to_pdfium();

        let mut document_0 = pdfium.create_new_pdf()?;

        {
            // Create three blank pages.

            for _ in 1..=3 {
                document_0
                    .pages_mut()
                    .create_page_at_end(PdfPagePaperSize::a4())?;
            }

            // Since we haven't retrieved any references to these pages, the index cache
            // should hold no entries for this document.

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document_0.handle()),
                0
            );

            // Check that the cache gets populated as we retrieve references to pages.

            let document_0_page_0 = document_0.pages().get(0)?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document_0.handle()),
                1
            );

            let document_0_page_1 = document_0.pages().get(1)?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document_0.handle()),
                2
            );

            let document_0_page_2 = document_0.pages().get(2)?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document_0.handle()),
                3
            );

            // Check the cached indices are correct.

            assert!(PdfPageIndexCache::lock()
                .get(document_0.handle(), document_0_page_0.page_handle())
                .is_some());
            assert!(
                PdfPageIndexCache::lock()
                    .get(document_0.handle(), document_0_page_0.page_handle())
                    .unwrap()
                    .index
                    == 0
            );

            assert!(PdfPageIndexCache::lock()
                .get(document_0.handle(), document_0_page_1.page_handle())
                .is_some());
            assert!(
                PdfPageIndexCache::lock()
                    .get(document_0.handle(), document_0_page_1.page_handle())
                    .unwrap()
                    .index
                    == 1
            );

            assert!(PdfPageIndexCache::lock()
                .get(document_0.handle(), document_0_page_2.page_handle())
                .is_some());
            assert!(
                PdfPageIndexCache::lock()
                    .get(document_0.handle(), document_0_page_2.page_handle())
                    .unwrap()
                    .index
                    == 2
            );

            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document_0.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document_0.handle())
                    .copied()
                    .unwrap(),
                2
            );

            // Now, while we still have references to those pages, let's create a second document
            // and make sure that references to the second document are also stored correctly.

            let mut document_1 = pdfium.create_new_pdf()?;

            {
                // Create four blank pages.

                for _ in 1..=4 {
                    document_1
                        .pages_mut()
                        .create_page_at_end(PdfPagePaperSize::a4())?;
                }

                // Since we haven't retrieved any references to these pages, the second document
                // should not yet contribute any entries to the cache, while the first document's
                // three entries remain untouched.

                assert_eq!(
                    PdfPageIndexCache::lock().count_for_document(document_0.handle()),
                    3
                );
                assert_eq!(
                    PdfPageIndexCache::lock().count_for_document(document_1.handle()),
                    0
                );

                // Check that the cache gets populated as we retrieve references to pages.

                let document_1_page_0 = document_1.pages().get(0)?;

                assert_eq!(
                    PdfPageIndexCache::lock().count_for_document(document_1.handle()),
                    1
                );

                let document_1_page_1 = document_1.pages().get(1)?;

                assert_eq!(
                    PdfPageIndexCache::lock().count_for_document(document_1.handle()),
                    2
                );

                let document_1_page_2 = document_1.pages().get(2)?;

                assert_eq!(
                    PdfPageIndexCache::lock().count_for_document(document_1.handle()),
                    3
                );

                let document_1_page_3 = document_1.pages().get(3)?;

                assert_eq!(
                    PdfPageIndexCache::lock().count_for_document(document_1.handle()),
                    4
                );

                // Check the cached indices are correct.

                assert!(PdfPageIndexCache::lock()
                    .get(document_1.handle(), document_1_page_0.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document_1.handle(), document_1_page_0.page_handle())
                        .unwrap()
                        .index,
                    0
                );

                assert!(PdfPageIndexCache::lock()
                    .get(document_1.handle(), document_1_page_1.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document_1.handle(), document_1_page_1.page_handle())
                        .unwrap()
                        .index,
                    1
                );

                assert!(PdfPageIndexCache::lock()
                    .get(document_1.handle(), document_1_page_2.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document_1.handle(), document_1_page_2.page_handle())
                        .unwrap()
                        .index,
                    2
                );

                assert!(PdfPageIndexCache::lock()
                    .get(document_1.handle(), document_1_page_3.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document_1.handle(), document_1_page_3.page_handle())
                        .unwrap()
                        .index,
                    3
                );

                assert!(PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document_1.handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .documents_by_maximum_index
                        .get(&document_1.handle())
                        .copied()
                        .unwrap(),
                    3
                );
            }

            // At this point, the pages from document_1 have been dropped. Those pages should
            // have been removed from the cache.

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document_1.handle()),
                0
            );
            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document_0.handle()),
                3
            );
        }

        // At this point, the pages from document_0 have been dropped. Those pages should
        // have been removed from the cache; the cache should now hold no entries for it.

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document_0.handle()),
            0
        );

        Ok(())
    }

    #[test]
    fn test_get_invalid_page() -> Result<(), PdfiumError> {
        let pdfium = test_bind_to_pdfium();

        let mut document = pdfium.create_new_pdf()?;

        let page_handle = {
            // Create a new page...

            let page = document
                .pages_mut()
                .create_page_at_start(PdfPagePaperSize::a4())?;

            // ... confirm the index of the page is cached...

            assert!(PdfPageIndexCache::lock()
                .get(document.handle(), page.page_handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .get(document.handle(), page.page_handle())
                    .unwrap()
                    .index,
                0
            );

            // ... and return the handle of the page.

            page.page_handle()
        };

        // At this point, the page itself has been dropped, so the page handle is no longer valid.
        // Attempting to retrieve the cached index for the page should return None.

        assert!(PdfPageIndexCache::lock()
            .get(document.handle(), page_handle)
            .is_none());

        Ok(())
    }

    #[test]
    fn test_insert_pages_at_index() -> Result<(), PdfiumError> {
        // Create a document with 100 pages, caching the index position of each page.

        let pdfium = test_bind_to_pdfium();

        let mut document = pdfium.create_new_pdf()?;

        // To cache the index position of each page, we have to hold a reference to each page.
        // We use a Vec to do this. Create the Vec inside a sub-scope, to ensure its lifetime
        // is shorter than document and pdfium.

        {
            let mut pages = Vec::new();

            for _ in 1..=100 {
                pages.push(
                    document
                        .pages_mut()
                        .create_page_at_end(PdfPagePaperSize::a4())?,
                );
            }

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                100
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                99
            );

            for (index, page) in pages.iter().enumerate() {
                assert!(PdfPageIndexCache::lock()
                    .get(document.handle(), page.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document.handle(), page.page_handle())
                        .unwrap()
                        .index,
                    index as PdfPageIndex
                );
            }

            // Our cache now holds 100 index positions. Insert a new page at the start of the document...

            let inserted = document
                .pages_mut()
                .create_page_at_start(PdfPagePaperSize::a4())?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                101
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                100
            );

            assert!(PdfPageIndexCache::lock()
                .get(document.handle(), inserted.page_handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .get(document.handle(), inserted.page_handle())
                    .unwrap()
                    .index,
                0
            );

            // ... and check that the index positions for all other pages have correctly shuffled down.

            for (index, page) in pages.iter().enumerate() {
                assert!(PdfPageIndexCache::lock()
                    .get(document.handle(), page.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document.handle(), page.page_handle())
                        .unwrap()
                        .index,
                    index as PdfPageIndex + 1
                );
            }

            // Our cache now holds 101 index positions. Insert a new page at position 50...

            let inserted = document
                .pages_mut()
                .create_page_at_index(PdfPagePaperSize::a4(), 50)?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                102
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                101
            );

            assert!(PdfPageIndexCache::lock()
                .get(document.handle(), inserted.page_handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .get(document.handle(), inserted.page_handle())
                    .unwrap()
                    .index,
                50
            );

            // ... and check that the index positions for pages before position 50 _haven't_ changed,
            // while the index positions for pages _after_ position 50 _have_ shuffled down.

            for (index, page) in pages.iter().enumerate() {
                // We compare against an index position of 49 rather than 50 because we've already
                // inserted one page at the beginning of the document. This insertion at index position
                // 50 is our _second_ insertion into the page sequence.

                if index < 49 {
                    assert!(PdfPageIndexCache::lock()
                        .get(document.handle(), page.page_handle())
                        .is_some());
                    assert_eq!(
                        PdfPageIndexCache::lock()
                            .get(document.handle(), page.page_handle())
                            .unwrap()
                            .index,
                        index as PdfPageIndex + 1
                    );
                }

                if index > 49 {
                    assert!(PdfPageIndexCache::lock()
                        .get(document.handle(), page.page_handle())
                        .is_some());
                    assert_eq!(
                        PdfPageIndexCache::lock()
                            .get(document.handle(), page.page_handle())
                            .unwrap()
                            .index,
                        index as PdfPageIndex + 2
                    );
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_delete_pages_at_index() -> Result<(), PdfiumError> {
        // Create a document with 100 pages, caching the index position of each page.

        let pdfium = test_bind_to_pdfium();

        let mut document = pdfium.create_new_pdf()?;

        // To cache the index position of each page, we have to hold a reference to each page.
        // We use a Vec to do this. Create the Vec inside a sub-scope, to ensure its lifetime
        // is shorter than document and pdfium.

        {
            let mut pages = Vec::new();

            for _ in 1..=100 {
                pages.push(Some(
                    document
                        .pages_mut()
                        .create_page_at_end(PdfPagePaperSize::a4())?,
                ));
            }

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                100
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                99
            );

            for (index, page) in pages.iter().enumerate() {
                assert!(page.is_some());

                let document = document.handle();
                let page = page.as_ref().unwrap().page_handle();

                assert!(PdfPageIndexCache::lock().get(document, page).is_some());
                assert_eq!(
                    PdfPageIndexCache::lock().get(document, page).unwrap().index,
                    index as PdfPageIndex
                );
            }

            // Our cache now holds 100 index positions. Delete the page at the start of the document...

            pages.first_mut().unwrap().take().unwrap().delete()?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                99
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                98
            );

            // ... and check that the index positions for all other pages have correctly shuffled up.

            for (index, page) in pages.iter().enumerate() {
                if index == 0 {
                    // This page no longer exists.

                    assert!(page.is_none());
                } else {
                    assert!(page.is_some());

                    let document = document.handle();
                    let page = page.as_ref().unwrap().page_handle();

                    assert!(PdfPageIndexCache::lock().get(document, page).is_some());
                    assert_eq!(
                        PdfPageIndexCache::lock().get(document, page).unwrap().index,
                        index as PdfPageIndex - 1
                    );
                }
            }

            // Our cache now holds 99 index positions. Delete the page at index position 50...

            pages.get_mut(50).unwrap().take().unwrap().delete()?;

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                98
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                97
            );

            // ... and check that the index positions for pages before position 50 _haven't_ changed,
            // while the index positions for pages _after_ position 50 _have_ shuffled up.

            for (index, page) in pages.iter().enumerate() {
                if index == 0 || index == 50 {
                    // This page no longer exists.

                    assert!(page.is_none());
                } else if index < 50 {
                    assert!(page.is_some());

                    let document = document.handle();
                    let page = page.as_ref().unwrap().page_handle();

                    assert!(PdfPageIndexCache::lock().get(document, page).is_some());
                    assert_eq!(
                        PdfPageIndexCache::lock().get(document, page).unwrap().index,
                        index as PdfPageIndex - 1
                    );
                } else if index > 50 {
                    assert!(page.is_some());

                    let document = document.handle();
                    let page = page.as_ref().unwrap().page_handle();

                    assert!(PdfPageIndexCache::lock().get(document, page).is_some());
                    assert_eq!(
                        PdfPageIndexCache::lock().get(document, page).unwrap().index,
                        index as PdfPageIndex - 2
                    );
                }
            }
        }

        Ok(())
    }

    #[test]
    fn test_pathological_delete_all_pages() -> Result<(), PdfiumError> {
        // Create a document with 100 pages, caching the index position of each page,
        // then delete all one hundred pages, testing the cached maximum page index
        // after each deletion.

        let pdfium = test_bind_to_pdfium();

        let mut document = pdfium.create_new_pdf()?;

        // To cache the index position of each page, we have to hold a reference to each page.
        // We use a Vec to do this. Create the Vec inside a sub-scope, to ensure its lifetime
        // is shorter than document and pdfium.

        {
            let mut pages = Vec::new();

            for _ in 1..=100 {
                pages.push(
                    document
                        .pages_mut()
                        .create_page_at_end(PdfPagePaperSize::a4())?,
                );
            }

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                100
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_some());
            assert_eq!(
                PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .copied()
                    .unwrap(),
                99
            );

            for (index, page) in pages.iter().enumerate() {
                assert!(PdfPageIndexCache::lock()
                    .get(document.handle(), page.page_handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .get(document.handle(), page.page_handle())
                        .unwrap()
                        .index,
                    index as PdfPageIndex
                );
            }

            // Our cache now holds 100 index positions. Delete all 100 pages.

            for index in (0..100).rev() {
                assert!(PdfPageIndexCache::lock()
                    .documents_by_maximum_index
                    .get(&document.handle())
                    .is_some());
                assert_eq!(
                    PdfPageIndexCache::lock()
                        .documents_by_maximum_index
                        .get(&document.handle())
                        .copied()
                        .unwrap(),
                    index
                );

                PdfPageIndexCache::lock().delete(document.handle(), index, 1);

                if index > 0 {
                    assert!(PdfPageIndexCache::lock()
                        .documents_by_maximum_index
                        .get(&document.handle())
                        .is_some());
                    assert_eq!(
                        PdfPageIndexCache::lock()
                            .documents_by_maximum_index
                            .get(&document.handle())
                            .copied()
                            .unwrap(),
                        index - 1
                    );
                }
            }

            // All pages are now deleted.

            assert_eq!(
                PdfPageIndexCache::lock().count_for_document(document.handle()),
                0
            );
            assert!(PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document.handle())
                .is_none());
        }

        Ok(())
    }

    #[test]
    fn count_for_document_isolates_entries_by_document_handle() {
        // This is the pure-logic pin for the fix. It exercises the shared PAGE_INDEX_CACHE
        // directly with synthetic (non-pdfium) document and page handles, so it needs no native
        // library and runs deterministically from a single thread.
        //
        // It reproduces the isolation flaw described in the issue: once two different documents
        // have entries in the shared cache at the same time, a GLOBAL `pages_by_index.len()`
        // assertion sees BOTH documents' entries, while a document-scoped `count_for_document`
        // sees only the entries it owns. Before the fix, the suite asserted on the global length
        // and therefore failed whenever a second document was present (which, under the default
        // multi-threaded `cargo test`, a concurrent test routinely supplies). After the fix, the
        // assertions are document-scoped and hold no matter what else is in the cache.
        //
        // I use large, distinctive synthetic handle values so they cannot collide with entries
        // left by any other test, and I remove my own entries at the end so I leave the shared
        // cache exactly as I found it.

        use crate::bindgen::{FPDF_DOCUMENT, FPDF_PAGE};
        use crate::pdf::document::page::PdfPageContentRegenerationStrategy;

        let document_a = 0xA000_0000usize as FPDF_DOCUMENT;
        let document_b = 0xB000_0000usize as FPDF_DOCUMENT;

        let a_page_0 = 0xA000_0001usize as FPDF_PAGE;
        let a_page_1 = 0xA000_0002usize as FPDF_PAGE;
        let b_page_0 = 0xB000_0001usize as FPDF_PAGE;
        let b_page_1 = 0xB000_0002usize as FPDF_PAGE;
        let b_page_2 = 0xB000_0003usize as FPDF_PAGE;

        // Snapshot the whole-cache length before I add anything. Other tests may or may not have
        // entries present depending on scheduling, so I only ever reason about the DELTA.

        let global_before = PdfPageIndexCache::lock().pages_by_index.len();

        // Document A contributes two entries; document B contributes three. They coexist in the
        // shared cache, exactly the situation that breaks a global-length assertion.

        for (page, index) in [(a_page_0, 0), (a_page_1, 1)] {
            PdfPageIndexCache::cache_props_for_page(
                document_a,
                page,
                index,
                PdfPageContentRegenerationStrategy::AutomaticOnEveryChange,
            );
        }

        for (page, index) in [(b_page_0, 0), (b_page_1, 1), (b_page_2, 2)] {
            PdfPageIndexCache::cache_props_for_page(
                document_b,
                page,
                index,
                PdfPageContentRegenerationStrategy::AutomaticOnEveryChange,
            );
        }

        // The document-scoped counts are exact and isolated: each document sees only its own
        // entries, regardless of the other document or of any foreign entries in the cache.

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document_a),
            2,
            "count_for_document must see only document A's two entries"
        );
        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document_b),
            3,
            "count_for_document must see only document B's three entries"
        );

        // Meanwhile the GLOBAL length has grown by all five entries at once. This is what the old
        // per-document assertions were really measuring, which is why they could not survive a
        // second document being present. A document-scoped assertion of `== 3` on document B would
        // pass here; a global assertion of `== 3` would see `global_before + 5` and fail.

        let global_after = PdfPageIndexCache::lock().pages_by_index.len();

        assert_eq!(
            global_after - global_before,
            5,
            "the shared cache holds entries for BOTH documents at once, so a global length \
             assertion is not isolated to a single document"
        );

        // Clean up my synthetic entries so the shared cache is left untouched for other tests.

        for (document, page) in [
            (document_a, a_page_0),
            (document_a, a_page_1),
            (document_b, b_page_0),
            (document_b, b_page_1),
            (document_b, b_page_2),
        ] {
            PdfPageIndexCache::remove_index_for_page(document, page);
        }

        assert_eq!(PdfPageIndexCache::lock().count_for_document(document_a), 0);
        assert_eq!(PdfPageIndexCache::lock().count_for_document(document_b), 0);
    }

    #[test]
    fn dropping_a_documents_last_page_clears_its_maximum_index() {
        // Pure-logic pin for the document-scoped maximum index search in `remove()`. It drives the
        // shared cache with synthetic (non-pdfium) handles, so it needs no native library and is
        // deterministic from a single thread.
        //
        // `remove()` used to ask whether `indices_by_page` was globally empty before deciding that
        // a document had no cached indices left. Whenever any other document still held entries,
        // the answer was "not empty" and the document fell through to a search that found nothing
        // of its own, seeded its maximum at zero, and wrote that back. The entry then outlived the
        // document forever. pdfium reuses `FPDF_DOCUMENT` addresses, so the next document handed
        // out at the same address inherited a maximum index it never set, which silently skews the
        // index shuffling `insert()` and `delete()` do for it.
        //
        // I keep a second document's entry alive for the whole test, because a globally empty
        // cache is exactly the case the old code got right.

        use crate::bindgen::{FPDF_DOCUMENT, FPDF_PAGE};
        use crate::pdf::document::page::PdfPageContentRegenerationStrategy;

        let document = 0xD000_0000usize as FPDF_DOCUMENT;
        let other_document = 0xD100_0000usize as FPDF_DOCUMENT;

        let page = 0xD000_0001usize as FPDF_PAGE;
        let other_page = 0xD100_0001usize as FPDF_PAGE;

        PdfPageIndexCache::cache_props_for_page(
            other_document,
            other_page,
            7,
            PdfPageContentRegenerationStrategy::AutomaticOnEveryChange,
        );

        PdfPageIndexCache::cache_props_for_page(
            document,
            page,
            3,
            PdfPageContentRegenerationStrategy::AutomaticOnEveryChange,
        );

        assert_eq!(
            PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document)
                .copied(),
            Some(3)
        );

        // Drop this document's only cached page while the other document still has one.

        PdfPageIndexCache::remove_index_for_page(document, page);

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document),
            0,
            "the page entry itself should be gone"
        );

        assert_eq!(
            PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&document)
                .copied(),
            None,
            "a document with no cached page indices left must drop out of \
             documents_by_maximum_index, not linger with a maximum index of zero for whichever \
             document pdfium next allocates at the same address"
        );

        // The other document is untouched by all of this.

        assert_eq!(
            PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&other_document)
                .copied(),
            Some(7)
        );

        // Clean up my synthetic entry so the shared cache is left exactly as I found it.

        PdfPageIndexCache::remove_index_for_page(other_document, other_page);

        assert_eq!(
            PdfPageIndexCache::lock()
                .documents_by_maximum_index
                .get(&other_document)
                .copied(),
            None
        );
    }

    #[test]
    fn document_scoped_counts_isolate_across_live_documents() -> Result<(), PdfiumError> {
        // The end-to-end counterpart of the pure-logic pin above, driven through the real pdfium
        // page APIs. I hold two live documents at once so the shared cache provably contains
        // entries for both, then confirm that a document-scoped count reports each document's own
        // entries exactly, while the global length reflects the sum of both. This is the assertion
        // shape the whole suite now uses, and it is correct no matter what other tests do to the
        // shared cache under the default multi-threaded `cargo test`.

        let pdfium = test_bind_to_pdfium();

        // Document A: two pages, both held live.

        let mut document_a = pdfium.create_new_pdf()?;

        for _ in 1..=2 {
            document_a
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())?;
        }

        let _a_page_0 = document_a.pages().get(0)?;
        let _a_page_1 = document_a.pages().get(1)?;

        // Document B: three pages, all held live.

        let mut document_b = pdfium.create_new_pdf()?;

        for _ in 1..=3 {
            document_b
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())?;
        }

        let _b_page_0 = document_b.pages().get(0)?;
        let _b_page_1 = document_b.pages().get(1)?;
        let _b_page_2 = document_b.pages().get(2)?;

        // Document-scoped counts are exact and isolated even though both documents (and possibly
        // others from concurrent tests) are present in the shared cache at the same time.

        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document_a.handle()),
            2
        );
        assert_eq!(
            PdfPageIndexCache::lock().count_for_document(document_b.handle()),
            3
        );

        Ok(())
    }

    #[test]
    fn parallel_document_scoped_counts_do_not_abort() {
        // This is the regression pin for the actual reported symptom: the default multi-threaded
        // `cargo test` aborting the whole binary. Several threads each create a document, take
        // live page references, then assert on the cache. With the document-scoped
        // `count_for_document` assertion, every thread sees only the entries it owns regardless of
        // what the other threads are doing, so none of them panics, nothing poisons the shared
        // mutex, and no page drop re-panics inside a destructor. The old global
        // `pages_by_index.len() == 1` assertion would instead observe the other threads' entries,
        // panic while holding the lock guard, poison the mutex, and escalate to a process abort.
        //
        // The number each thread expects is the number of live `PdfPage` values it is holding, not
        // the number of pages in its document. The cache is keyed by `(FPDF_DOCUMENT, FPDF_PAGE)`
        // and pdfium hands back a distinct `FPDF_PAGE` for the page created by `FPDFPage_New` and
        // for the page later loaded from index 0 by `FPDF_LoadPage`, so holding both means this
        // document contributes exactly two entries. I pin that assumption with an explicit handle
        // comparison, so if a future pdfium ever starts returning the same handle for both the
        // failure says so instead of looking like a cache accounting bug.

        use std::thread;

        let handles: Vec<_> = (0..8)
            .map(|_| {
                thread::spawn(|| -> Result<(), PdfiumError> {
                    let pdfium = test_bind_to_pdfium();

                    let mut document = pdfium.create_new_pdf()?;

                    let page = document
                        .pages_mut()
                        .create_page_at_start(PdfPagePaperSize::a4())?;

                    // Take a second live reference to the same page position, so this thread
                    // contributes more than one entry to the shared cache.

                    let page_ref = document.pages().get(0)?;

                    assert_ne!(
                        page.page_handle(),
                        page_ref.page_handle(),
                        "pdfium returned one FPDF_PAGE for both the created and the loaded page, \
                         so the document-scoped count below is no longer two"
                    );

                    // Document-scoped assertion: this thread only ever sees its own two entries,
                    // so it is stable under concurrency. I read the count out of the guard into a
                    // local first, so that a failure here unwinds with the guard already released.

                    let count = PdfPageIndexCache::lock().count_for_document(document.handle());

                    assert_eq!(
                        count, 2,
                        "document-scoped count is not isolated to this thread's own document"
                    );

                    Ok(())
                })
            })
            .collect();

        // Join every thread before asserting on any of them. Bailing out on the first failure
        // would drop the remaining join handles undetached, leaving worker threads still creating
        // and closing documents while the next test runs, which contaminates its cache counts.

        let results: Vec<_> = handles.into_iter().map(|handle| handle.join()).collect();

        for result in results {
            result
                .expect("worker thread panicked, indicating the cache assertions are not isolated")
                .expect("worker thread returned a pdfium error");
        }
    }

    #[test]
    fn poisoned_cache_mutex_does_not_abort_later_page_drops() {
        // This pins the recovery behaviour of `PdfPageIndexCache::lock()`. It used to unwrap the
        // `LockResult`, so a single panic anywhere under the cache guard poisoned the
        // process-global mutex for the rest of the run. `PdfPage::drop_impl` takes that same lock,
        // so the very next page drop then panicked inside a destructor, and Rust turns a panic
        // during unwinding into a non-unwinding abort: one failed assertion killed the whole test
        // binary with SIGABRT and hid every test that had not run yet.
        //
        // I poison the mutex on purpose, then check the two things that used to break: that
        // `lock()` still hands back a usable guard, and that a full create/read/drop cycle through
        // the public API still keeps the cache accurate. The `catch_unwind` check comes first so
        // that a regression fails cleanly here, before any live `PdfPage` exists whose destructor
        // could turn the failure into an abort.
        //
        // Poisoning a `Mutex` is permanent and process-global, but it is inert now that `lock()`
        // recovers from it, so this leaves nothing behind for the other tests. The panic message
        // the poisoning thread prints is expected output, not a failure.

        use std::panic::{catch_unwind, AssertUnwindSafe};
        use std::thread;

        let poisoner = thread::spawn(|| {
            let _guard = PdfPageIndexCache::lock();

            panic!("deliberately poisoning PAGE_INDEX_CACHE; this panic is expected");
        });

        assert!(
            poisoner.join().is_err(),
            "the poisoning thread was supposed to panic while holding the cache guard"
        );

        assert!(
            PAGE_INDEX_CACHE.lock().is_err(),
            "PAGE_INDEX_CACHE should be poisoned at this point"
        );

        let recovered = catch_unwind(AssertUnwindSafe(|| {
            PdfPageIndexCache::lock().pages_by_index.len()
        }));

        assert!(
            recovered.is_ok(),
            "PdfPageIndexCache::lock() panicked on a poisoned mutex; every later PdfPage drop \
             would panic inside its destructor and abort the process"
        );

        // Now the end-to-end path: creating a page, reading its cached entry, and dropping it all
        // go through the poisoned mutex.

        let pdfium = test_bind_to_pdfium();

        let mut document = pdfium
            .create_new_pdf()
            .expect("could not create a document through the poisoned cache mutex");

        {
            let _page = document
                .pages_mut()
                .create_page_at_start(PdfPagePaperSize::a4())
                .expect("could not create a page through the poisoned cache mutex");

            let count = PdfPageIndexCache::lock().count_for_document(document.handle());

            assert_eq!(
                count, 1,
                "the page was not cached through the poisoned mutex"
            );
        }

        let count = PdfPageIndexCache::lock().count_for_document(document.handle());

        assert_eq!(
            count, 0,
            "the page drop did not reach the cache through the poisoned mutex"
        );
    }
}
