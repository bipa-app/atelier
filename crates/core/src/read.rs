use atelier_diff_core::PackageId;

use crate::error::Error;

/// The most bytes one read returns; also the default window. No unbounded
/// responses exist on the surface.
pub const MAX_READ_WINDOW: usize = 50_000;

/// Where in the text a read's content sits, in bytes of the text read —
/// the projection's for a projected document, the document's own otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadWindow {
    pub start: usize,
    pub end: usize,
    pub total: usize,
}

/// One windowed read: bounded content plus the cursor to continue from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    pub content: String,
    pub window: ReadWindow,
    /// The byte offset the next read starts at; `None` when this window
    /// reached the end.
    pub next: Option<usize>,
    /// The package whose projection was read; `None` for plain text.
    pub projected_by: Option<PackageId>,
}

/// The window size a caller asked for, bounded to `1..=MAX_READ_WINDOW`.
pub(crate) fn window_size(requested: Option<usize>) -> Result<usize, Error> {
    match requested {
        None => Ok(MAX_READ_WINDOW),
        Some(size) if (1..=MAX_READ_WINDOW).contains(&size) => Ok(size),
        Some(_) => Err(Error::WindowTooLarge {
            max: MAX_READ_WINDOW,
        }),
    }
}

/// The window of `text` starting at byte `start`, at most `size` bytes,
/// both edges snapped to character boundaries so the content stays valid
/// UTF-8. A start at or past the end yields an empty window at the end.
pub(crate) fn window_text(
    text: &str,
    start: usize,
    size: usize,
    projected_by: Option<PackageId>,
) -> ReadResult {
    let total = text.len();
    let start = snap_forward(text, start.min(total));
    let end = snap_back(text, start.saturating_add(size).min(total)).max(start);
    ReadResult {
        content: text[start..end].to_owned(),
        window: ReadWindow { start, end, total },
        next: (end < total).then_some(end),
        projected_by,
    }
}

fn snap_forward(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

fn snap_back(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::{MAX_READ_WINDOW, window_size, window_text};

    #[test]
    fn windows_chain_through_the_text_and_reassemble_it() {
        let text = "0123456789";

        let first = window_text(text, 0, 4, None);
        assert_eq!(first.content, "0123");
        assert_eq!((first.window.start, first.window.end), (0, 4));
        assert_eq!(first.window.total, 10);
        assert_eq!(first.next, Some(4));

        let second = window_text(text, 4, 4, None);
        assert_eq!(second.content, "4567");
        assert_eq!(second.next, Some(8));

        let last = window_text(text, 8, 4, None);
        assert_eq!(last.content, "89");
        assert_eq!(last.next, None);

        assert_eq!(
            format!("{}{}{}", first.content, second.content, last.content),
            text
        );
    }

    #[test]
    fn window_edges_snap_to_character_boundaries() {
        // é is two bytes; a window ending inside it must retreat.
        let text = "ané";
        let clipped = window_text(text, 0, 3, None);
        assert_eq!(clipped.content, "an");
        assert_eq!(clipped.next, Some(2));

        let rest = window_text(text, 2, 3, None);
        assert_eq!(rest.content, "é");
        assert_eq!(rest.next, None);
    }

    #[test]
    fn a_start_past_the_end_yields_an_empty_final_window() {
        let result = window_text("abc", 10, 5, None);
        assert_eq!(result.content, "");
        assert_eq!((result.window.start, result.window.end), (3, 3));
        assert_eq!(result.next, None);
    }

    #[test]
    fn window_sizes_are_bounded() {
        assert_eq!(window_size(None).unwrap(), MAX_READ_WINDOW);
        assert_eq!(window_size(Some(1)).unwrap(), 1);
        assert!(window_size(Some(0)).is_err());
        assert!(window_size(Some(MAX_READ_WINDOW + 1)).is_err());
    }
}
