use std::sync::Arc;

/// A host-owned runtime seam that is initialized once and can be released at
/// shutdown. Unlike [`std::sync::OnceLock`], clearing the live value does not
/// reopen the initialization latch, so a late producer can never resurrect a
/// link after the host has drained its children.
pub struct RuntimeLink<T> {
    pub(super) state: std::sync::RwLock<RuntimeLinkState<T>>,
}

pub(super) struct RuntimeLinkState<T> {
    pub(super) initialized: bool,
    pub(super) value: Option<T>,
}

impl<T> Default for RuntimeLink<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RuntimeLink<T> {
    /// Construct an empty, permanently set-once link.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: std::sync::RwLock::new(RuntimeLinkState {
                initialized: false,
                value: None,
            }),
        }
    }

    /// Fill the link once. A second fill is rejected even after [`Self::clear`].
    pub fn set(&self, value: T) -> Result<(), T> {
        let mut state = self
            .state
            .write()
            .expect("runtime link lock poisoned while setting");
        if state.initialized {
            return Err(value);
        }
        state.initialized = true;
        state.value = Some(value);
        Ok(())
    }

    /// Release the live value and close the set-once latch. Clearing a link
    /// that was never filled also seals it against a late initializer.
    ///
    /// Taking the value in one scope and dropping it in the next is deliberate:
    /// a destructor may re-enter another runtime link, and must never run while
    /// this link's write lock is held.
    pub fn clear(&self) {
        let value = {
            let mut state = self
                .state
                .write()
                .expect("runtime link lock poisoned while clearing");
            // `clear` is also the shutdown seal for an optional link that was
            // never filled. Linearizing this flag under the same write lock as
            // `set` makes both race orders safe: either the value is installed
            // and then removed, or the later install is rejected.
            state.initialized = true;
            state.value.take()
        };
        drop(value);
    }

    /// Whether this link currently retains a live value. This inspection does
    /// not clone or invoke the stored value.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.state
            .read()
            .expect("runtime link lock poisoned while reading")
            .value
            .is_some()
    }

    /// Whether the latch is closed with no live value — the host either
    /// released this link or sealed it having never filled it.
    ///
    /// A [`std::sync::OnceLock`] has only one way to read empty, so a caller
    /// could treat `None` as "the host never installed this" and carry on.
    /// This type has two, and they mean opposite things: not-yet-filled is a
    /// host that simply has no such seam, while sealed-and-empty is a host
    /// that has drained. A gate whose absence means "allow" has to tell them
    /// apart or it fails open on the way down.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        let state = self
            .state
            .read()
            .expect("runtime link lock poisoned while reading");
        state.initialized && state.value.is_none()
    }
}

impl<T: ?Sized> RuntimeLink<Arc<T>> {
    /// Clone the current live `Arc`, if the link has been filled and not yet
    /// released. Runtime links intentionally accept only `Arc` reads: an
    /// arbitrary `T::clone` could run user code while the read lock is held.
    #[must_use]
    pub fn get(&self) -> Option<Arc<T>> {
        self.state
            .read()
            .expect("runtime link lock poisoned while reading")
            .value
            .as_ref()
            .map(Arc::clone)
    }
}
