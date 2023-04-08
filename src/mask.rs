pub(crate) const UNCOUNTED: bool = false;
pub(crate) const COUNTED: bool = true;

/// A bitmask storing whether a channel has been closed and the count of
/// currently senders.
pub(crate) struct Mask(usize);

impl Mask {
    pub(crate) const fn new() -> Self {
        Self(0)
    }

    /// Resets the closed bit and sender count to one (if counted).
    pub(crate) fn reset<const COUNTED: bool>(&mut self) {
        if COUNTED {
            self.0 += 2;
        } else {
            self.0 &= 0b1;
        }
    }

    /// Returns `true` if the closed bit is set.
    pub(crate) fn is_closed<const COUNTED: bool>(&self) -> bool {
        if COUNTED {
            self.0 & 0b1 == 1
        } else {
            self.0 == 1
        }
    }

    /// Sets the closed bit.
    pub(crate) fn close<const COUNTED: bool>(&mut self) {
        if COUNTED {
            self.0 |= 0b1;
        } else {
            self.0 = 1;
        }
    }

    // Increments the sender count by one.
    pub(crate) fn increase_sender_count(&mut self) {
        self.0 =
            self.0.checked_add(2).expect("cloning the sender would overflow the reference counter");
    }

    // Decrements the sender count by one and closes the queue if it reaches zero.
    #[must_use = "must react to final sender being dropped"]
    pub(crate) fn decrease_sender_count(&mut self) -> bool {
        // can not underflow, count starts at 2 and is only incremented while
        // ensuring no overflow can occur
        self.0 -= 2;

        // mask is 0 or 1: sender was last
        if self.0 < 2 {
            return self.set_closed_bit();
        }

        false
    }

    #[cold]
    pub(crate) fn set_closed_bit(&mut self) -> bool {
        self.0 = 1;
        true
    }
}
