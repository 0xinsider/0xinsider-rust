//! Following `next_cursor` to the end of a list, safely.
//!
//! Every list operation answers the envelope `{ object: "list", data, has_more,
//! next_cursor, meta }`. Feeds move while you read them: follow the cursor to
//! completion, never page by offset, and never total a partial walk.
//!
//! A page that says `has_more: true` with no usable `next_cursor`, or whose
//! cursor repeats one the walk already requested, is a broken response rather
//! than the end of the collection. The walk stops with [`PaginationError`]
//! before any duplicate request, instead of truncating or looping.
//!
//! ```no_run
//! # async fn run() -> oxinsider::Result<()> {
//! use oxinsider::{Client, ListWhaleTradesParams, models::Grade, pagination::Pager};
//!
//! let client = Client::from_env()?;
//! let mut pager = Pager::new(ListWhaleTradesParams::default().min_grade(Grade::A).limit(100));
//! while let Some(page) = pager.next_page(async |params| client.list_whale_trades(params).await).await? {
//!     for trade in &page.data {
//!         println!("{:?} {:?}", trade.size_usd, trade.side);
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::{HashSet, VecDeque};
use std::fmt;

use crate::error::Result;

/// How many cursors a walk remembers for the repeat check. Keeps a long walk's memory flat.
pub const CURSOR_HISTORY_LIMIT: usize = 1024;

/// A page of a cursor-paginated list.
pub trait ListPage {
    /// The row type.
    type Item;
    /// The rows on this page.
    fn items(&self) -> &[Self::Item];
    /// The rows on this page, owned.
    fn into_items(self) -> Vec<Self::Item>;
    /// Whether the collection continues past this page.
    fn has_more(&self) -> bool;
    /// The cursor for the next page.
    fn next_cursor(&self) -> Option<&str>;
}

/// Parameters of a list operation that take a `cursor`.
pub trait CursorParams {
    /// Set the cursor the next request starts from.
    fn set_cursor(&mut self, cursor: Option<String>);
}

/// Why a walk stopped on a broken page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PaginationErrorReason {
    /// `has_more: true` with no usable `next_cursor`.
    MissingCursor,
    /// `next_cursor` repeats a cursor the walk already requested.
    RepeatedCursor,
}

/// A page that cannot be continued. The page's rows were already returned (by
/// [`Pager::next_page`]) or are lost with the walk (by [`collect_all`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaginationError {
    /// Why the walk stopped.
    pub reason: PaginationErrorReason,
    /// Pages fetched, including the broken one.
    pub pages_fetched: u64,
    /// The cursor that fetched the broken page (`None` for the first page):
    /// resume from it later, or report it.
    pub cursor: Option<String>,
}

impl fmt::Display for PaginationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            PaginationErrorReason::MissingCursor => write!(
                f,
                "page {} says has_more but carries no next_cursor; the walk stopped instead of truncating",
                self.pages_fetched
            ),
            PaginationErrorReason::RepeatedCursor => write!(
                f,
                "page {} repeats a cursor the walk already requested; the walk stopped instead of looping",
                self.pages_fetched
            ),
        }
    }
}

impl std::error::Error for PaginationError {}

/// The state of one cursor walk: the pages fetched and the cursors requested.
#[derive(Debug, Clone, Default)]
pub struct CursorWalk {
    order: VecDeque<String>,
    seen: HashSet<String>,
    current: Option<String>,
    pages: u64,
}

impl CursorWalk {
    /// A walk that starts at `cursor` (`None` for the first page).
    pub fn starting_at(cursor: Option<String>) -> Self {
        let mut walk = Self::default();
        if let Some(cursor) = &cursor {
            walk.remember(cursor.clone());
        }
        walk.current = cursor;
        walk
    }

    /// Pages recorded so far.
    pub fn pages(&self) -> u64 {
        self.pages
    }

    /// Record a page and return the cursor for the next request, or `None`
    /// when the collection is complete.
    pub fn advance<P: ListPage + ?Sized>(&mut self, page: &P) -> Result<Option<String>, PaginationError> {
        self.pages += 1;
        if !page.has_more() {
            return Ok(None);
        }
        let error = |reason| PaginationError {
            reason,
            pages_fetched: self.pages,
            cursor: self.current.clone(),
        };
        let Some(next) = page.next_cursor().filter(|cursor| !cursor.trim().is_empty()) else {
            return Err(error(PaginationErrorReason::MissingCursor));
        };
        if self.seen.contains(next) {
            return Err(error(PaginationErrorReason::RepeatedCursor));
        }
        let next = next.to_owned();
        self.remember(next.clone());
        self.current = Some(next.clone());
        Ok(Some(next))
    }

    fn remember(&mut self, cursor: String) {
        if self.order.len() == CURSOR_HISTORY_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(cursor.clone());
        self.order.push_back(cursor);
    }
}

/// Fetches the pages of one list, one call at a time.
///
/// Pass the operation as an async closure: `async |params| client.list_whale_trades(params).await`.
/// For an operation with path arguments, capture them:
/// `async |params| client.get_market_holders(condition_id, params).await`.
#[derive(Debug, Clone)]
pub struct Pager<P> {
    params: P,
    walk: CursorWalk,
    done: bool,
}

impl<P: CursorParams> Pager<P> {
    /// A pager that starts from `params` (and its `cursor`, when set).
    pub fn new(params: P) -> Self {
        Self {
            params,
            walk: CursorWalk::default(),
            done: false,
        }
    }

    /// Resume a walk from a cursor you saved.
    pub fn resume(mut params: P, cursor: impl Into<String>) -> Self {
        let cursor = cursor.into();
        params.set_cursor(Some(cursor.clone()));
        Self {
            params,
            walk: CursorWalk::starting_at(Some(cursor)),
            done: false,
        }
    }

    /// Fetch the next page, or `None` once the collection is complete.
    ///
    /// A broken page (see [`PaginationError`]) is returned as that error; its
    /// rows are not delivered and the pager is finished. A failed request is
    /// returned unchanged and the pager can be asked again: it retries the same
    /// cursor.
    pub async fn next_page<R, F>(&mut self, fetch: F) -> Result<Option<R>>
    where
        R: ListPage,
        F: AsyncFnOnce(&P) -> Result<R>,
    {
        if self.done {
            return Ok(None);
        }
        let page = fetch(&self.params).await?;
        match self.walk.advance(&page) {
            Ok(Some(cursor)) => self.params.set_cursor(Some(cursor)),
            Ok(None) => self.done = true,
            Err(error) => {
                self.done = true;
                return Err(error.into());
            }
        }
        Ok(Some(page))
    }

    /// Whether the collection is complete.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The parameters the next request will send, including its cursor: save
    /// them to resume later.
    pub fn params(&self) -> &P {
        &self.params
    }

    /// Pages fetched so far.
    pub fn pages(&self) -> u64 {
        self.walk.pages()
    }
}

/// Every row of a list, following `next_cursor` to the end.
///
/// For a feed that grows while you read it, prefer [`Pager`] and decide where to stop.
pub async fn collect_all<P, R, F>(params: P, mut fetch: F) -> Result<Vec<R::Item>>
where
    P: CursorParams,
    R: ListPage,
    F: AsyncFnMut(&P) -> Result<R>,
{
    let mut pager = Pager::new(params);
    let mut items = Vec::new();
    while let Some(page) = pager.next_page(async |params| fetch(params).await).await? {
        items.extend(page.into_items());
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Page {
        data: Vec<u32>,
        has_more: bool,
        next_cursor: Option<&'static str>,
    }

    impl ListPage for Page {
        type Item = u32;
        fn items(&self) -> &[u32] {
            &self.data
        }
        fn into_items(self) -> Vec<u32> {
            self.data
        }
        fn has_more(&self) -> bool {
            self.has_more
        }
        fn next_cursor(&self) -> Option<&str> {
            self.next_cursor
        }
    }

    #[derive(Default, Clone, Debug)]
    struct Params {
        cursor: Option<String>,
    }

    impl CursorParams for Params {
        fn set_cursor(&mut self, cursor: Option<String>) {
            self.cursor = cursor;
        }
    }

    fn page(data: &[u32], has_more: bool, next: Option<&'static str>) -> Page {
        Page {
            data: data.to_vec(),
            has_more,
            next_cursor: next,
        }
    }

    #[test]
    fn has_more_false_ends_the_walk_whatever_the_cursor() {
        let mut walk = CursorWalk::default();
        assert_eq!(walk.advance(&page(&[1], false, Some("c1"))).unwrap(), None);
    }

    #[test]
    fn a_missing_cursor_is_an_error_not_the_end() {
        let mut walk = CursorWalk::default();
        let error = walk.advance(&page(&[1], true, None)).unwrap_err();
        assert_eq!(error.reason, PaginationErrorReason::MissingCursor);
        let error = walk.advance(&page(&[1], true, Some("  "))).unwrap_err();
        assert_eq!(error.reason, PaginationErrorReason::MissingCursor);
    }

    #[test]
    fn a_repeated_cursor_is_an_error_not_a_loop() {
        let mut walk = CursorWalk::default();
        assert_eq!(
            walk.advance(&page(&[1], true, Some("c1"))).unwrap().as_deref(),
            Some("c1")
        );
        assert_eq!(
            walk.advance(&page(&[2], true, Some("c2"))).unwrap().as_deref(),
            Some("c2")
        );
        let error = walk.advance(&page(&[3], true, Some("c1"))).unwrap_err();
        assert_eq!(error.reason, PaginationErrorReason::RepeatedCursor);
        assert_eq!(error.cursor.as_deref(), Some("c2"));
        assert_eq!(error.pages_fetched, 3);
    }

    #[test]
    fn a_resumed_walk_remembers_its_starting_cursor() {
        let mut walk = CursorWalk::starting_at(Some("c0".into()));
        let error = walk.advance(&page(&[1], true, Some("c0"))).unwrap_err();
        assert_eq!(error.reason, PaginationErrorReason::RepeatedCursor);
    }

    #[test]
    fn history_is_bounded() {
        let mut walk = CursorWalk::default();
        let cursors: Vec<&'static str> = (0..CURSOR_HISTORY_LIMIT + 10)
            .map(|i| &*Box::leak(format!("c{i}").into_boxed_str()))
            .collect();
        for cursor in &cursors {
            walk.advance(&page(&[], true, Some(cursor))).unwrap();
        }
        assert_eq!(walk.order.len(), CURSOR_HISTORY_LIMIT);
        assert_eq!(walk.seen.len(), CURSOR_HISTORY_LIMIT);
    }

    #[tokio::test]
    async fn collect_all_follows_cursors_and_sends_them() {
        let pages = std::sync::Mutex::new(vec![
            page(&[5], false, None),
            page(&[3, 4], true, Some("c2")),
            page(&[1, 2], true, Some("c1")),
        ]);
        let sent = std::sync::Mutex::new(Vec::new());
        let items = collect_all(Params::default(), async |params: &Params| {
            sent.lock().unwrap().push(params.cursor.clone());
            Ok(pages.lock().unwrap().pop().unwrap())
        })
        .await
        .unwrap();
        assert_eq!(items, vec![1, 2, 3, 4, 5]);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![None, Some("c1".to_owned()), Some("c2".to_owned())]
        );
    }
}
