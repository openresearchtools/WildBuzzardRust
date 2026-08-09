use std::fmt;
use std::ops::Range;

/// A validated byte-indexed selection in one UTF-8 address buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressSelection {
    anchor: usize,
    focus: usize,
}

impl AddressSelection {
    /// Creates a directional selection when both endpoints are UTF-8 boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`AddressEditError::InvalidSelection`] when either endpoint is
    /// out of range or splits a Unicode scalar.
    pub fn new(text: &str, anchor: usize, focus: usize) -> Result<Self, AddressEditError> {
        if anchor > text.len()
            || focus > text.len()
            || !text.is_char_boundary(anchor)
            || !text.is_char_boundary(focus)
        {
            return Err(AddressEditError::InvalidSelection {
                anchor,
                focus,
                text_bytes: text.len(),
            });
        }
        Ok(Self { anchor, focus })
    }

    /// Collapsed selection at the end of `text`.
    #[must_use]
    pub const fn at_end(text: &str) -> Self {
        Self {
            anchor: text.len(),
            focus: text.len(),
        }
    }

    /// Fixed endpoint from which an extended selection was started.
    #[must_use]
    pub const fn anchor(self) -> usize {
        self.anchor
    }

    /// Moving endpoint used by keyboard selection extension.
    #[must_use]
    pub const fn focus(self) -> usize {
        self.focus
    }

    /// Byte offset of the selection start.
    #[must_use]
    pub const fn start(self) -> usize {
        if self.anchor < self.focus {
            self.anchor
        } else {
            self.focus
        }
    }

    /// Byte offset of the selection end.
    #[must_use]
    pub const fn end(self) -> usize {
        if self.anchor > self.focus {
            self.anchor
        } else {
            self.focus
        }
    }

    /// Whether this is an insertion caret rather than a range.
    #[must_use]
    pub const fn is_collapsed(self) -> bool {
        self.anchor == self.focus
    }

    /// Standard range projection.
    #[must_use]
    pub const fn range(self) -> Range<usize> {
        self.start()..self.end()
    }
}

/// Direction for one Unicode-scalar cursor move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorMove {
    /// Move to the previous scalar boundary.
    Previous,
    /// Move to the next scalar boundary.
    Next,
    /// Move to byte offset zero.
    Start,
    /// Move to the final byte offset.
    End,
}

/// Bounded IME preedit retained separately from committed address text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddressPreedit {
    text: Box<str>,
    selection: Option<AddressSelection>,
}

impl AddressPreedit {
    /// Preedit text without committing it to the address buffer.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// IME-provided selection relative to the preedit text.
    #[must_use]
    pub const fn selection(&self) -> Option<AddressSelection> {
        self.selection
    }
}

/// Per-tab address draft, selection, and composition state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddressEditState {
    text: String,
    selection: AddressSelection,
    preedit: Option<AddressPreedit>,
    dirty: bool,
    maximum_bytes: usize,
}

impl AddressEditState {
    /// Creates an empty bounded editor.
    pub(crate) fn empty(maximum_bytes: usize) -> Self {
        Self {
            text: String::new(),
            selection: AddressSelection::at_end(""),
            preedit: None,
            dirty: false,
            maximum_bytes,
        }
    }

    /// Committed UTF-8 buffer, excluding any active preedit.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Current byte-indexed selection.
    #[must_use]
    pub const fn selection(&self) -> AddressSelection {
        self.selection
    }

    /// Active uncommitted IME preedit.
    #[must_use]
    pub const fn preedit(&self) -> Option<&AddressPreedit> {
        self.preedit.as_ref()
    }

    /// Whether local edits differ from the current requested navigation value.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Fixed byte limit for both committed and preedit text.
    #[must_use]
    pub const fn maximum_bytes(&self) -> usize {
        self.maximum_bytes
    }

    /// Replaces the buffer transactionally and marks it as user-edited.
    ///
    /// # Errors
    ///
    /// Returns [`AddressEditError::TooLong`] without changing the editor when
    /// `text` exceeds its fixed byte limit.
    pub fn set_text(&mut self, text: &str) -> Result<(), AddressEditError> {
        self.check_length(text.len())?;
        self.text.clear();
        self.text.push_str(text);
        self.selection = AddressSelection::at_end(&self.text);
        self.preedit = None;
        self.dirty = true;
        Ok(())
    }

    /// Sets a checked selection without changing text.
    ///
    /// # Errors
    ///
    /// Returns [`AddressEditError::InvalidSelection`] when an endpoint is not
    /// a boundary in the current committed buffer.
    pub fn set_selection(&mut self, selection: AddressSelection) -> Result<(), AddressEditError> {
        self.selection = AddressSelection::new(&self.text, selection.anchor, selection.focus)?;
        Ok(())
    }

    /// Selects the entire committed buffer.
    pub fn select_all(&mut self) {
        self.selection = AddressSelection {
            anchor: 0,
            focus: self.text.len(),
        };
    }

    /// Inserts committed text at the current selection transactionally.
    ///
    /// # Errors
    ///
    /// Returns [`AddressEditError::TooLong`] without changing the editor when
    /// the resulting buffer would exceed its fixed byte limit.
    pub fn insert(&mut self, inserted: &str) -> Result<(), AddressEditError> {
        let removed = self.selection.end() - self.selection.start();
        let next_len = self
            .text
            .len()
            .checked_sub(removed)
            .and_then(|len| len.checked_add(inserted.len()))
            .ok_or(AddressEditError::TooLong {
                actual: usize::MAX,
                maximum: self.maximum_bytes,
            })?;
        self.check_length(next_len)?;
        let insertion_end = self.selection.start().checked_add(inserted.len()).ok_or(
            AddressEditError::TooLong {
                actual: usize::MAX,
                maximum: self.maximum_bytes,
            },
        )?;
        self.text.replace_range(self.selection.range(), inserted);
        self.selection = AddressSelection {
            anchor: insertion_end,
            focus: insertion_end,
        };
        self.preedit = None;
        self.dirty = true;
        Ok(())
    }

    /// Deletes the selected range or the previous Unicode scalar.
    pub fn backspace(&mut self) {
        if !self.selection.is_collapsed() {
            self.delete_selection();
            return;
        }
        let caret = self.selection.focus;
        let previous = self.text[..caret]
            .char_indices()
            .next_back()
            .map_or(caret, |(index, _)| index);
        if previous != caret {
            self.text.replace_range(previous..caret, "");
            self.selection = AddressSelection {
                anchor: previous,
                focus: previous,
            };
            self.dirty = true;
        }
        self.preedit = None;
    }

    /// Deletes the selected range or the next Unicode scalar.
    pub fn delete_forward(&mut self) {
        if !self.selection.is_collapsed() {
            self.delete_selection();
            return;
        }
        let caret = self.selection.focus;
        let next = self.text[caret..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(offset, _)| caret + offset);
        if next != caret {
            self.text.replace_range(caret..next, "");
            self.dirty = true;
        }
        self.preedit = None;
    }

    /// Moves the caret, collapsing any existing selection in the direction of travel.
    pub fn move_cursor(&mut self, movement: CursorMove, extend: bool) {
        let anchor = self.selection.anchor;
        let target = if !extend && !self.selection.is_collapsed() {
            match movement {
                CursorMove::Previous => self.selection.start(),
                CursorMove::Next => self.selection.end(),
                CursorMove::Start => 0,
                CursorMove::End => self.text.len(),
            }
        } else {
            let base = self.selection.focus;
            match movement {
                CursorMove::Previous => self.text[..base]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index),
                CursorMove::Next => self.text[base..]
                    .char_indices()
                    .nth(1)
                    .map_or(self.text.len(), |(offset, _)| base + offset),
                CursorMove::Start => 0,
                CursorMove::End => self.text.len(),
            }
        };
        self.selection = if extend {
            AddressSelection {
                anchor,
                focus: target,
            }
        } else {
            AddressSelection {
                anchor: target,
                focus: target,
            }
        };
        self.preedit = None;
    }

    /// Updates bounded composition text without modifying the committed buffer.
    ///
    /// # Errors
    ///
    /// Returns [`AddressEditError`] when the preedit exceeds the byte limit or
    /// its optional selection is not a valid UTF-8 range.
    pub fn set_preedit(
        &mut self,
        text: &str,
        selection: Option<Range<usize>>,
    ) -> Result<(), AddressEditError> {
        self.check_length(text.len())?;
        let selection = selection
            .map(|selection| AddressSelection::new(text, selection.start, selection.end))
            .transpose()?;
        self.preedit = if text.is_empty() {
            None
        } else {
            Some(AddressPreedit {
                text: text.into(),
                selection,
            })
        };
        Ok(())
    }

    /// Cancels any uncommitted composition.
    pub fn clear_preedit(&mut self) {
        self.preedit = None;
    }

    pub(crate) fn accept_navigation_value(&mut self, value: &str) {
        debug_assert!(value.len() <= self.maximum_bytes);
        self.text.clear();
        self.text.push_str(value);
        self.selection = AddressSelection::at_end(&self.text);
        self.preedit = None;
        self.dirty = false;
    }

    pub(crate) fn revert_to(&mut self, value: &str) {
        self.accept_navigation_value(value);
        self.select_all();
    }

    fn delete_selection(&mut self) {
        let start = self.selection.start();
        self.text.replace_range(self.selection.range(), "");
        self.selection = AddressSelection {
            anchor: start,
            focus: start,
        };
        self.preedit = None;
        self.dirty = true;
    }

    fn check_length(&self, actual: usize) -> Result<(), AddressEditError> {
        if actual > self.maximum_bytes {
            Err(AddressEditError::TooLong {
                actual,
                maximum: self.maximum_bytes,
            })
        } else {
            Ok(())
        }
    }
}

/// Rejected edit which leaves the previous address state intact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressEditError {
    /// UTF-8 byte limit exceeded.
    TooLong { actual: usize, maximum: usize },
    /// An endpoint was outside the string or split a Unicode scalar.
    InvalidSelection {
        anchor: usize,
        focus: usize,
        text_bytes: usize,
    },
}

impl fmt::Display for AddressEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { actual, maximum } => {
                write!(
                    formatter,
                    "address text has {actual} bytes; maximum is {maximum}"
                )
            }
            Self::InvalidSelection {
                anchor,
                focus,
                text_bytes,
            } => write!(
                formatter,
                "address selection anchor/focus {anchor}/{focus} is invalid for {text_bytes} UTF-8 bytes"
            ),
        }
    }
}

impl std::error::Error for AddressEditError {}

#[cfg(test)]
mod tests {
    use super::{AddressEditError, AddressEditState, AddressSelection, CursorMove};

    #[test]
    fn shift_selection_retains_direction_across_multibyte_boundaries() {
        let mut edit = AddressEditState::empty(64);
        edit.set_text("a🦅b").unwrap();

        edit.move_cursor(CursorMove::Previous, true);
        assert_eq!(edit.selection().anchor(), 6);
        assert_eq!(edit.selection().focus(), 5);
        assert_eq!(edit.selection().range(), 5..6);

        edit.move_cursor(CursorMove::Previous, true);
        assert_eq!(edit.selection().anchor(), 6);
        assert_eq!(edit.selection().focus(), 1);
        assert_eq!(edit.selection().range(), 1..6);

        edit.move_cursor(CursorMove::Next, true);
        assert_eq!(edit.selection().anchor(), 6);
        assert_eq!(edit.selection().focus(), 5);
        assert_eq!(edit.selection().range(), 5..6);

        edit.move_cursor(CursorMove::Next, true);
        assert!(edit.selection().is_collapsed());
        assert_eq!(edit.selection().anchor(), 6);
        assert_eq!(edit.selection().focus(), 6);

        edit.set_selection(AddressSelection::new(edit.text(), 1, 1).unwrap())
            .unwrap();
        edit.move_cursor(CursorMove::Next, true);
        assert_eq!(edit.selection().anchor(), 1);
        assert_eq!(edit.selection().focus(), 5);
        edit.move_cursor(CursorMove::Previous, true);
        assert_eq!(
            edit.selection(),
            AddressSelection::new(edit.text(), 1, 1).unwrap()
        );
    }

    #[test]
    fn oversized_insert_is_transactional() {
        let mut edit = AddressEditState::empty(6);
        edit.set_text("🦅a").unwrap();
        edit.set_selection(AddressSelection::new(edit.text(), 5, 5).unwrap())
            .unwrap();
        let before = edit.clone();

        assert_eq!(
            edit.insert("bc"),
            Err(AddressEditError::TooLong {
                actual: 7,
                maximum: 6,
            })
        );
        assert_eq!(edit, before);
    }

    #[test]
    fn deleting_reverse_selection_uses_ordered_utf8_range() {
        let mut edit = AddressEditState::empty(64);
        edit.set_text("x🦅y").unwrap();
        edit.set_selection(AddressSelection::new(edit.text(), 5, 1).unwrap())
            .unwrap();
        edit.backspace();
        assert_eq!(edit.text(), "xy");
        assert_eq!(
            edit.selection(),
            AddressSelection::new(edit.text(), 1, 1).unwrap()
        );
    }

    #[test]
    fn plain_arrow_collapses_selection_without_moving_past_it() {
        let mut edit = AddressEditState::empty(64);
        edit.set_text("a🦅b").unwrap();
        edit.set_selection(AddressSelection::new(edit.text(), 5, 1).unwrap())
            .unwrap();

        edit.move_cursor(CursorMove::Previous, false);
        assert_eq!(
            edit.selection(),
            AddressSelection::new(edit.text(), 1, 1).unwrap()
        );

        edit.set_selection(AddressSelection::new(edit.text(), 1, 5).unwrap())
            .unwrap();
        edit.move_cursor(CursorMove::Next, false);
        assert_eq!(
            edit.selection(),
            AddressSelection::new(edit.text(), 5, 5).unwrap()
        );
    }
}
