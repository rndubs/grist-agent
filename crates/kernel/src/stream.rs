//! A minimal single-item `Stream` so `Provider::complete_stream` has a default without a
//! `futures-util` dependency.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

/// A stream that yields exactly one item and then ends.
pub struct Once<T>(Option<T>);

impl<T: Unpin> Unpin for Once<T> {}

impl<T: Unpin> Stream for Once<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<T>> {
        Poll::Ready(self.get_mut().0.take())
    }
}

/// Build a one-item stream.
pub fn once<T>(item: T) -> Once<T> {
    Once(Some(item))
}
