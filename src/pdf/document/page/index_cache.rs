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

    /// Returns the number of `pages_by_index` entries currently cached for the given raw document
    /// handle. The [PAGE_INDEX_CACHE] is process-global and shared by every open document, so
    /// counting entries scoped to a single document is the only way a test can assert on cache
    /// contents without depending on what other, concurrently running tests happen to have cached.
    ///
    /// This is a guard-free accessor: it acquires the lock internally and returns a plain `usize`,
    /// so a caller never holds a [MutexGuard] across an assertion made on the result. A failing
    /// assertion therefore cannot poison [PAGE_INDEX_CACHE] while a guard is still live.
    #[cfg(test)]
    #[inline]
    fn count_for_document(document: FPDF_DOCUMENT) -> usize {
        Self::lock()
            .pages_by_index
            .keys()
            .filter(|(cached_document, _)| *cached_document == document)
            .count()
    }

    /// Returns the currently cached maximum [PdfPageIndex] for the given raw document handle, if
    /// any. Guard-free accessor; see [PdfPageIndexCache::count_for_document].
    #[cfg(test)]
    #[inline]
    fn maximum_index_for_document(document: FPDF_DOCUMENT) -> Option<PdfPageIndex> {
        Self::lock()
            .documents_by_maximum_index
            .get(&document)
            .copied()
    }

    /// Returns the raw page handle cached in the reverse (`indices_by_page`) map for the given raw
    /// document handle and [PdfPageIndex], if any. Guard-free accessor; see
    /// [PdfPageIndexCache::count_for_document].
    #[cfg(test)]
    #[inline]
    fn page_for_index(document: FPDF_DOCUMENT, index: PdfPageIndex) -> Option<FPDF_PAGE> {
        Self::lock()
            .indices_by_page
            .get(&(document, index))
            .copied()
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

        assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 0);

        {
            // Now let's create a blank page and get a handle to it...

            let _page = document
                .pages_mut()
                .create_page_at_start(PdfPagePaperSize::a4())?;

            // ... and confirm the cache updated.

            assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 1);
        }

        // The page has dropped out of scope. Confirm the cache got cleaned up.

        assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 0);

        // Get a new handle to the page...

        let _page = document.pages().first();

        // ... and confirm the cache updated.

        assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 1);

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
                PdfPageIndexCache::count_for_document(document_0.handle()),
                0
            );

            // Check that the cache gets populated as we retrieve references to pages.

            let document_0_page_0 = document_0.pages().get(0)?;

            assert_eq!(
                PdfPageIndexCache::count_for_document(document_0.handle()),
                1
            );

            let document_0_page_1 = document_0.pages().get(1)?;

            assert_eq!(
                PdfPageIndexCache::count_for_document(document_0.handle()),
                2
            );

            let document_0_page_2 = document_0.pages().get(2)?;

            assert_eq!(
                PdfPageIndexCache::count_for_document(document_0.handle()),
                3
            );

            // Check the cached indices are correct.

            assert!(PdfPageIndexCache::get_index_for_page(
                document_0.handle(),
                document_0_page_0.page_handle()
            )
            .is_some());
            assert!(
                PdfPageIndexCache::get_index_for_page(
                    document_0.handle(),
                    document_0_page_0.page_handle()
                )
                .unwrap()
                    == 0
            );

            assert!(PdfPageIndexCache::get_index_for_page(
                document_0.handle(),
                document_0_page_1.page_handle()
            )
            .is_some());
            assert!(
                PdfPageIndexCache::get_index_for_page(
                    document_0.handle(),
                    document_0_page_1.page_handle()
                )
                .unwrap()
                    == 1
            );

            assert!(PdfPageIndexCache::get_index_for_page(
                document_0.handle(),
                document_0_page_2.page_handle()
            )
            .is_some());
            assert!(
                PdfPageIndexCache::get_index_for_page(
                    document_0.handle(),
                    document_0_page_2.page_handle()
                )
                .unwrap()
                    == 2
            );

            assert!(PdfPageIndexCache::maximum_index_for_document(document_0.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document_0.handle()).unwrap(),
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
                    PdfPageIndexCache::count_for_document(document_0.handle()),
                    3
                );
                assert_eq!(
                    PdfPageIndexCache::count_for_document(document_1.handle()),
                    0
                );

                // Check that the cache gets populated as we retrieve references to pages.

                let document_1_page_0 = document_1.pages().get(0)?;

                assert_eq!(
                    PdfPageIndexCache::count_for_document(document_1.handle()),
                    1
                );

                let document_1_page_1 = document_1.pages().get(1)?;

                assert_eq!(
                    PdfPageIndexCache::count_for_document(document_1.handle()),
                    2
                );

                let document_1_page_2 = document_1.pages().get(2)?;

                assert_eq!(
                    PdfPageIndexCache::count_for_document(document_1.handle()),
                    3
                );

                let document_1_page_3 = document_1.pages().get(3)?;

                assert_eq!(
                    PdfPageIndexCache::count_for_document(document_1.handle()),
                    4
                );

                // Check the cached indices are correct.

                assert!(PdfPageIndexCache::get_index_for_page(
                    document_1.handle(),
                    document_1_page_0.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(
                        document_1.handle(),
                        document_1_page_0.page_handle()
                    )
                    .unwrap(),
                    0
                );

                assert!(PdfPageIndexCache::get_index_for_page(
                    document_1.handle(),
                    document_1_page_1.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(
                        document_1.handle(),
                        document_1_page_1.page_handle()
                    )
                    .unwrap(),
                    1
                );

                assert!(PdfPageIndexCache::get_index_for_page(
                    document_1.handle(),
                    document_1_page_2.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(
                        document_1.handle(),
                        document_1_page_2.page_handle()
                    )
                    .unwrap(),
                    2
                );

                assert!(PdfPageIndexCache::get_index_for_page(
                    document_1.handle(),
                    document_1_page_3.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(
                        document_1.handle(),
                        document_1_page_3.page_handle()
                    )
                    .unwrap(),
                    3
                );

                assert!(
                    PdfPageIndexCache::maximum_index_for_document(document_1.handle()).is_some()
                );
                assert_eq!(
                    PdfPageIndexCache::maximum_index_for_document(document_1.handle()).unwrap(),
                    3
                );
            }

            // At this point, the pages from document_1 have been dropped. Those pages should
            // have been removed from the cache.

            assert_eq!(
                PdfPageIndexCache::count_for_document(document_1.handle()),
                0
            );
            assert_eq!(
                PdfPageIndexCache::count_for_document(document_0.handle()),
                3
            );
        }

        // At this point, the pages from document_0 have been dropped. Those pages should
        // have been removed from the cache; the cache should now hold no entries for it.

        assert_eq!(
            PdfPageIndexCache::count_for_document(document_0.handle()),
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

            assert!(
                PdfPageIndexCache::get_index_for_page(document.handle(), page.page_handle())
                    .is_some()
            );
            assert_eq!(
                PdfPageIndexCache::get_index_for_page(document.handle(), page.page_handle())
                    .unwrap(),
                0
            );

            // ... and return the handle of the page.

            page.page_handle()
        };

        // At this point, the page itself has been dropped, so the page handle is no longer valid.
        // Attempting to retrieve the cached index for the page should return None.

        assert!(PdfPageIndexCache::get_index_for_page(document.handle(), page_handle).is_none());

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
                PdfPageIndexCache::count_for_document(document.handle()),
                100
            );
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                99
            );

            for (index, page) in pages.iter().enumerate() {
                assert!(PdfPageIndexCache::get_index_for_page(
                    document.handle(),
                    page.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(document.handle(), page.page_handle())
                        .unwrap(),
                    index as PdfPageIndex
                );
            }

            // Our cache now holds 100 index positions. Insert a new page at the start of the document...

            let inserted = document
                .pages_mut()
                .create_page_at_start(PdfPagePaperSize::a4())?;

            assert_eq!(
                PdfPageIndexCache::count_for_document(document.handle()),
                101
            );
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                100
            );

            assert!(PdfPageIndexCache::get_index_for_page(
                document.handle(),
                inserted.page_handle()
            )
            .is_some());
            assert_eq!(
                PdfPageIndexCache::get_index_for_page(document.handle(), inserted.page_handle())
                    .unwrap(),
                0
            );

            // ... and check that the index positions for all other pages have correctly shuffled down.

            for (index, page) in pages.iter().enumerate() {
                assert!(PdfPageIndexCache::get_index_for_page(
                    document.handle(),
                    page.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(document.handle(), page.page_handle())
                        .unwrap(),
                    index as PdfPageIndex + 1
                );
            }

            // Our cache now holds 101 index positions. Insert a new page at position 50...

            let inserted = document
                .pages_mut()
                .create_page_at_index(PdfPagePaperSize::a4(), 50)?;

            assert_eq!(
                PdfPageIndexCache::count_for_document(document.handle()),
                102
            );
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                101
            );

            assert!(PdfPageIndexCache::get_index_for_page(
                document.handle(),
                inserted.page_handle()
            )
            .is_some());
            assert_eq!(
                PdfPageIndexCache::get_index_for_page(document.handle(), inserted.page_handle())
                    .unwrap(),
                50
            );

            // ... and check that the index positions for pages before position 50 _haven't_ changed,
            // while the index positions for pages _after_ position 50 _have_ shuffled down.

            for (index, page) in pages.iter().enumerate() {
                // We compare against an index position of 49 rather than 50 because we've already
                // inserted one page at the beginning of the document. This insertion at index position
                // 50 is our _second_ insertion into the page sequence.

                if index < 49 {
                    assert!(PdfPageIndexCache::get_index_for_page(
                        document.handle(),
                        page.page_handle()
                    )
                    .is_some());
                    assert_eq!(
                        PdfPageIndexCache::get_index_for_page(
                            document.handle(),
                            page.page_handle()
                        )
                        .unwrap(),
                        index as PdfPageIndex + 1
                    );
                }

                if index > 49 {
                    assert!(PdfPageIndexCache::get_index_for_page(
                        document.handle(),
                        page.page_handle()
                    )
                    .is_some());
                    assert_eq!(
                        PdfPageIndexCache::get_index_for_page(
                            document.handle(),
                            page.page_handle()
                        )
                        .unwrap(),
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
                PdfPageIndexCache::count_for_document(document.handle()),
                100
            );
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                99
            );

            for (index, page) in pages.iter().enumerate() {
                assert!(page.is_some());

                let document = document.handle();
                let page = page.as_ref().unwrap().page_handle();

                assert!(PdfPageIndexCache::get_index_for_page(document, page).is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(document, page).unwrap(),
                    index as PdfPageIndex
                );
            }

            // Our cache now holds 100 index positions. Delete the page at the start of the document...

            pages.first_mut().unwrap().take().unwrap().delete()?;

            assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 99);
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
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

                    assert!(PdfPageIndexCache::get_index_for_page(document, page).is_some());
                    assert_eq!(
                        PdfPageIndexCache::get_index_for_page(document, page).unwrap(),
                        index as PdfPageIndex - 1
                    );
                }
            }

            // Our cache now holds 99 index positions. Delete the page at index position 50...

            pages.get_mut(50).unwrap().take().unwrap().delete()?;

            assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 98);
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
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

                    assert!(PdfPageIndexCache::get_index_for_page(document, page).is_some());
                    assert_eq!(
                        PdfPageIndexCache::get_index_for_page(document, page).unwrap(),
                        index as PdfPageIndex - 1
                    );
                } else if index > 50 {
                    assert!(page.is_some());

                    let document = document.handle();
                    let page = page.as_ref().unwrap().page_handle();

                    assert!(PdfPageIndexCache::get_index_for_page(document, page).is_some());
                    assert_eq!(
                        PdfPageIndexCache::get_index_for_page(document, page).unwrap(),
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
                PdfPageIndexCache::count_for_document(document.handle()),
                100
            );
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
            assert_eq!(
                PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                99
            );

            for (index, page) in pages.iter().enumerate() {
                assert!(PdfPageIndexCache::get_index_for_page(
                    document.handle(),
                    page.page_handle()
                )
                .is_some());
                assert_eq!(
                    PdfPageIndexCache::get_index_for_page(document.handle(), page.page_handle())
                        .unwrap(),
                    index as PdfPageIndex
                );
            }

            // Our cache now holds 100 index positions. Delete all 100 pages.

            for index in (0..100).rev() {
                assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some());
                assert_eq!(
                    PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                    index
                );

                PdfPageIndexCache::delete_pages_at_index(document.handle(), index, 1);

                if index > 0 {
                    assert!(
                        PdfPageIndexCache::maximum_index_for_document(document.handle()).is_some()
                    );
                    assert_eq!(
                        PdfPageIndexCache::maximum_index_for_document(document.handle()).unwrap(),
                        index - 1
                    );
                }
            }

            // All pages are now deleted.

            assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 0);
            assert!(PdfPageIndexCache::maximum_index_for_document(document.handle()).is_none());
        }

        Ok(())
    }

    #[test]
    fn count_for_document_isolates_entries_by_document_handle() {
        // Pure-logic pin for the fix. It exercises the shared PAGE_INDEX_CACHE directly with
        // synthetic (non-pdfium) document and page handles, so it needs no native library and runs
        // deterministically from a single thread.
        //
        // It reproduces the isolation flaw described in the issue: once two different documents have
        // entries in the shared cache at the same time, a global `pages_by_index.len()` assertion
        // sees BOTH documents' entries, while a document-scoped `count_for_document` sees only the
        // entries it owns. Before the fix, the suite asserted on the global length and therefore
        // failed whenever a second document was present (which, under the default multi-threaded
        // `cargo test`, a concurrent test routinely supplies). After the fix, the assertions are
        // document-scoped and hold no matter what else is in the cache.
        //
        // Large, distinctive synthetic handle values keep this test's own entries easy to identify
        // and remove. The assertions below only ever reason about the two synthetic documents this
        // test owns; they make no claim about foreign entries left by other tests.

        use crate::bindgen::{FPDF_DOCUMENT, FPDF_PAGE};
        use crate::pdf::document::page::PdfPageContentRegenerationStrategy;

        let document_a = 0xA000_0000usize as FPDF_DOCUMENT;
        let document_b = 0xB000_0000usize as FPDF_DOCUMENT;

        let a_page_0 = 0xA000_0001usize as FPDF_PAGE;
        let a_page_1 = 0xA000_0002usize as FPDF_PAGE;
        let b_page_0 = 0xB000_0001usize as FPDF_PAGE;
        let b_page_1 = 0xB000_0002usize as FPDF_PAGE;
        let b_page_2 = 0xB000_0003usize as FPDF_PAGE;

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
        // entries, regardless of the other document or of any foreign entries left in the shared
        // cache by concurrent tests. These accessors are guard-free, so a failing assertion here
        // holds no cache mutex guard across its panic.

        assert_eq!(
            PdfPageIndexCache::count_for_document(document_a),
            2,
            "count_for_document must see only document A's two entries"
        );
        assert_eq!(
            PdfPageIndexCache::count_for_document(document_b),
            3,
            "count_for_document must see only document B's three entries"
        );

        // The reverse (indices_by_page) map is likewise document-scoped: each (document, index)
        // pair resolves back to the page handle originally cached against it.

        assert_eq!(
            PdfPageIndexCache::page_for_index(document_a, 0),
            Some(a_page_0)
        );
        assert_eq!(
            PdfPageIndexCache::page_for_index(document_a, 1),
            Some(a_page_1)
        );
        assert_eq!(
            PdfPageIndexCache::page_for_index(document_b, 2),
            Some(b_page_2)
        );

        // Remove every synthetic entry this test added, across both documents, so the page entries
        // it introduced do not linger in the shared cache. `remove_index_for_page` clears both the
        // forward (`pages_by_index`) and reverse (`indices_by_page`) entries for each page.

        for (document, page) in [
            (document_a, a_page_0),
            (document_a, a_page_1),
            (document_b, b_page_0),
            (document_b, b_page_1),
            (document_b, b_page_2),
        ] {
            PdfPageIndexCache::remove_index_for_page(document, page);
        }

        // The page entries are gone for both documents.

        assert_eq!(PdfPageIndexCache::count_for_document(document_a), 0);
        assert_eq!(PdfPageIndexCache::count_for_document(document_b), 0);
        assert!(PdfPageIndexCache::page_for_index(document_a, 0).is_none());
        assert!(PdfPageIndexCache::page_for_index(document_b, 2).is_none());
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
            PdfPageIndexCache::count_for_document(document_a.handle()),
            2
        );
        assert_eq!(
            PdfPageIndexCache::count_for_document(document_b.handle()),
            3
        );

        Ok(())
    }

    // This test drives pdfium's FFI from eight threads at once, which is only sound when the
    // `thread_safe` feature serializes access to the library. It is gated accordingly so that it
    // neither compiles nor runs without that feature (where the concurrent FFI would be undefined
    // behaviour).
    #[cfg(feature = "thread_safe")]
    #[test]
    fn parallel_document_scoped_counts_do_not_abort() {
        // Regression pin for the actual reported symptom: the default multi-threaded `cargo test`
        // aborting the whole binary. Several threads each create a document, take a live page
        // reference, then assert on the cache. With the document-scoped `count_for_document`
        // assertion, every thread sees only its own single entry regardless of what the other
        // threads are doing, so none of them panics, nothing poisons the shared mutex, and no page
        // drop re-panics inside a destructor. The old global `pages_by_index.len() == 1` assertion
        // would instead observe the other threads' entries, panic while holding the lock guard,
        // poison the mutex, and escalate to a process abort. Every worker thread is joined and
        // required to have succeeded.

        use std::thread;

        for _ in 0..8 {
            let handle = thread::spawn(|| -> Result<(), PdfiumError> {
                let pdfium = test_bind_to_pdfium();

                let mut document = pdfium.create_new_pdf()?;

                document
                    .pages_mut()
                    .create_page_at_start(PdfPagePaperSize::a4())?;

                // Take a live reference so this thread contributes an entry to the shared cache.

                let page = document.pages().get(0)?;

                // Document-scoped assertion: this thread only ever sees its own entry, so it
                // is stable under concurrency. The accessor is guard-free, so even a regression
                // here could not poison the cache mutex.

                assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 1);

                drop(page);

                assert_eq!(PdfPageIndexCache::count_for_document(document.handle()), 0);

                Ok(())
            });

            handle.join().unwrap().unwrap();
        }
    }

    #[test]
    fn failed_document_scoped_assertion_does_not_poison_the_cache_mutex() {
        // Proves the failure mode targeted by this fix is gone, without needing pdfium. The
        // original bug was: a document-scoped assertion evaluated against a value pulled from a
        // live `MutexGuard` would, on failure, panic while that guard was still held, poison
        // PAGE_INDEX_CACHE, and then the panicking test's own unwind would drop a live PdfPage
        // whose destructor re-locks the now-poisoned mutex and double-panics into a process abort.
        //
        // Here a deliberately wrong document-scoped assertion is run inside `catch_unwind` using
        // the guard-free `count_for_document` accessor. Because that accessor releases the lock
        // before the value is compared, the panic happens with no guard held, so the mutex must NOT
        // be poisoned: a following `lock()` still succeeds and a following `count_for_document`
        // still works. That is exactly what keeps a subsequent PdfPage drop safe rather than
        // abort-inducing.

        use crate::bindgen::{FPDF_DOCUMENT, FPDF_PAGE};
        use crate::pdf::document::page::PdfPageContentRegenerationStrategy;

        let document = 0xC000_0000usize as FPDF_DOCUMENT;
        let page_0 = 0xC000_0001usize as FPDF_PAGE;
        let page_1 = 0xC000_0002usize as FPDF_PAGE;

        for (page, index) in [(page_0, 0), (page_1, 1)] {
            PdfPageIndexCache::cache_props_for_page(
                document,
                page,
                index,
                PdfPageContentRegenerationStrategy::AutomaticOnEveryChange,
            );
        }

        // Suppress the panic backtrace that the deliberately failing assertion would otherwise
        // print, so the test output is not misleading, then run the failing assertion in isolation.

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Deliberately WRONG: the document has two entries, not 999. This models a genuine
            // future regression in a document-scoped cache assertion.
            assert_eq!(
                PdfPageIndexCache::count_for_document(document),
                999,
                "deliberately failing document-scoped assertion"
            );
        }));

        std::panic::set_hook(previous_hook);

        // The assertion must have failed...
        assert!(
            result.is_err(),
            "the deliberately wrong assertion was expected to panic"
        );

        // ... but because no MutexGuard was held across that panic, the cache mutex must not be
        // poisoned. A poisoned mutex is precisely what turned a failed assertion into a process
        // abort via the re-locking PdfPage destructor.
        assert!(
            super::PAGE_INDEX_CACHE.lock().is_ok(),
            "the cache mutex must not be poisoned by a failed document-scoped assertion"
        );

        // And the cache is still usable after the failed assertion.
        assert_eq!(PdfPageIndexCache::count_for_document(document), 2);

        // Clean up this test's synthetic entries.

        for page in [page_0, page_1] {
            PdfPageIndexCache::remove_index_for_page(document, page);
        }

        assert_eq!(PdfPageIndexCache::count_for_document(document), 0);
    }

    // -----------------------------------------------------------------------
    // Kept across the upstream sync (2026-09-12).
    //
    // Upstream's own #266 and #267 work adopted the rest of this module's
    // tests, so the merge took its version wholesale. These two have no
    // upstream counterpart and still pin behaviour only this fork has:
    // `lock()` recovering from a poisoned mutex (upstream is still
    // `.lock().unwrap()`), and the document-scoped maximum-index search in
    // `remove()`.
    // -----------------------------------------------------------------------

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
