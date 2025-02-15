use std::{
    cell::RefCell,
    fmt, mem, str,
    sync::atomic::{self, AtomicUsize, Ordering},
};

use owning_ref::OwningHandle;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) use tracing_core::span::{Attributes, Current, Id, Record};
use tracing_core::{dispatcher, Metadata};

pub struct Span<'a> {
    lock: OwningHandle<RwLockReadGuard<'a, Slab>, RwLockReadGuard<'a, Slot>>,
}

/// Represents the `Subscriber`'s view of the current span context to a
/// formatter.
#[derive(Debug)]
pub struct Context<'a, N> {
    store: &'a Store,
    new_visitor: &'a N,
}

/// Stores data associated with currently-active spans.
#[derive(Debug)]
pub(crate) struct Store {
    // Active span data is stored in a slab of span slots. Each slot has its own
    // read-write lock to guard against concurrent modification to its data.
    // Thus, we can modify any individual slot by acquiring a read lock on the
    // slab, and using that lock to acquire a write lock on the slot we wish to
    // modify. It is only necessary to acquire the write lock here when the
    // slab itself has to be modified (i.e., to allocate more slots).
    inner: RwLock<Slab>,

    // The head of the slab's "free list".
    next: AtomicUsize,
}

#[derive(Debug)]
pub(crate) struct Data {
    parent: Option<Id>,
    metadata: &'static Metadata<'static>,
    ref_count: AtomicUsize,
    is_empty: bool,
}

#[derive(Debug)]
struct Slab {
    slab: Vec<RwLock<Slot>>,
}

#[derive(Debug)]
struct Slot {
    fields: String,
    span: State,
}

#[derive(Debug)]
enum State {
    Full(Data),
    Empty(usize),
}

thread_local! {
    static CONTEXT: RefCell<Vec<Id>> = RefCell::new(vec![]);
}

macro_rules! debug_panic {
    ($($args:tt)*) => {
        #[cfg(debug_assertions)] {
            if !std::thread::panicking() {
                panic!($($args)*)
            }
        }
    }
}

// ===== impl Span =====

impl<'a> Span<'a> {
    pub fn name(&self) -> &'static str {
        match self.lock.span {
            State::Full(ref data) => data.metadata.name(),
            State::Empty(_) => unreachable!(),
        }
    }

    pub fn metadata(&self) -> &'static Metadata<'static> {
        match self.lock.span {
            State::Full(ref data) => data.metadata,
            State::Empty(_) => unreachable!(),
        }
    }

    pub fn fields(&self) -> &str {
        self.lock.fields.as_ref()
    }

    pub fn parent(&self) -> Option<&Id> {
        match self.lock.span {
            State::Full(ref data) => data.parent.as_ref(),
            State::Empty(_) => unreachable!(),
        }
    }

    #[inline(always)]
    fn with_parent<'store, F, E>(
        self,
        my_id: &Id,
        last_id: Option<&Id>,
        f: &mut F,
        store: &'store Store,
    ) -> Result<(), E>
    where
        F: FnMut(&Id, Span<'_>) -> Result<(), E>,
    {
        if let Some(parent_id) = self.parent() {
            if Some(parent_id) != last_id {
                if let Some(parent) = store.get(parent_id) {
                    parent.with_parent(parent_id, Some(my_id), f, store)?;
                } else {
                    debug_panic!("missing span for {:?}; this is a bug", parent_id);
                }
            }
        }
        f(my_id, self)
    }
}

impl<'a> fmt::Debug for Span<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Span")
            .field("name", &self.name())
            .field("parent", &self.parent())
            .field("metadata", self.metadata())
            .field("fields", &self.fields())
            .finish()
    }
}

// ===== impl Context =====

impl<'a, N> Context<'a, N> {
    /// Applies a function to each span in the current trace context.
    ///
    /// The function is applied in order, beginning with the root of the trace,
    /// and ending with the current span. If the function returns an error,
    /// this will short-circuit.
    ///
    /// If invoked from outside of a span, the function will not be applied.
    ///
    /// Note that if we are currently unwinding, this will do nothing, rather
    /// than potentially causing a double panic.
    pub fn visit_spans<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(&Id, Span<'_>) -> Result<(), E>,
    {
        CONTEXT
            .try_with(|current| {
                if let Some(id) = current.borrow().last() {
                    if let Some(span) = self.store.get(id) {
                        // with_parent uses the call stack to visit the span
                        // stack in reverse order, without having to allocate
                        // a buffer.
                        return span.with_parent(id, None, &mut f, self.store);
                    } else {
                        debug_panic!("missing span for {:?}; this is a bug", id);
                    }
                }
                Ok(())
            })
            .unwrap_or(Ok(()))
    }

    /// Executes a closure with the reference to the current span.
    pub fn with_current<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce((&Id, Span<'_>)) -> R,
    {
        // If the lock is poisoned or the thread local has already been
        // destroyed, we might be in the middle of unwinding, so this
        // will just do nothing rather than cause a double panic.
        CONTEXT
            .try_with(|current| {
                if let Some(id) = current.borrow().last() {
                    if let Some(span) = self.store.get(id) {
                        return Some(f((id, span)));
                    } else {
                        debug_panic!("missing span for {:?}, this is a bug", id);
                    }
                }
                None
            })
            .ok()?
    }

    pub(crate) fn new(store: &'a Store, new_visitor: &'a N) -> Self {
        Self { store, new_visitor }
    }

    /// Returns a new visitor that formats span fields to the provided writer.
    /// The visitor configuration is provided by the subscriber.
    pub fn new_visitor<'writer>(
        &self,
        writer: &'writer mut dyn fmt::Write,
        is_empty: bool,
    ) -> N::Visitor
    where
        N: super::NewVisitor<'writer>,
    {
        self.new_visitor.make(writer, is_empty)
    }
}

#[inline]
fn idx_to_id(idx: usize) -> Id {
    Id::from_u64(idx as u64 + 1)
}

#[inline]
fn id_to_idx(id: &Id) -> usize {
    id.into_u64() as usize - 1
}

impl Store {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Store {
            inner: RwLock::new(Slab {
                slab: Vec::with_capacity(capacity),
            }),
            next: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn current(&self) -> Option<Id> {
        CONTEXT
            .try_with(|current| current.borrow().last().map(|span| self.clone_span(span)))
            .ok()?
    }

    pub(crate) fn push(&self, id: &Id) {
        let _ = CONTEXT.try_with(|current| {
            let mut current = current.borrow_mut();
            if current.contains(id) {
                // Ignore duplicate enters.
                return;
            }
            current.push(self.clone_span(id));
        });
    }

    pub(crate) fn pop(&self, expected_id: &Id) {
        let id = CONTEXT
            .try_with(|current| {
                let mut current = current.borrow_mut();
                if current.last() == Some(expected_id) {
                    current.pop()
                } else {
                    None
                }
            })
            .ok()
            .and_then(|i| i);
        if let Some(id) = id {
            let _ = self.drop_span(id);
        }
    }

    /// Inserts a new span with the given data and fields into the slab,
    /// returning an ID for that span.
    ///
    /// If there are empty slots in the slab previously allocated for spans
    /// which have since been closed, the allocation and span ID of the most
    /// recently emptied span will be reused. Otherwise, a new allocation will
    /// be added to the slab.
    #[inline]
    pub(crate) fn new_span<N>(&self, attrs: &Attributes<'_>, new_visitor: &N) -> Id
    where
        N: for<'a> super::NewVisitor<'a>,
    {
        let mut span = Some(Data::new(attrs, self));

        // The slab's free list is a modification of Treiber's lock-free stack,
        // using slab indices instead of pointers, and with a provision for
        // growing the slab when needed.
        //
        // In order to insert a new span into the slab, we "pop" the next free
        // index from the stack.
        loop {
            // Acquire a snapshot of the head of the free list.
            let head = self.next.load(Ordering::Relaxed);

            {
                // Try to insert the span without modifying the overall
                // structure of the stack.
                let this = self.inner.read();

                // Can we insert without reallocating?
                if head < this.slab.len() {
                    // If someone else is writing to the head slot, we need to
                    // acquire a new snapshot!
                    if let Some(mut slot) = this.slab[head].try_write() {
                        // Is the slot we locked actually empty? If not, fall
                        // through and try to grow the slab.
                        if let Some(next) = slot.next() {
                            // Is our snapshot still valid?
                            if self.next.compare_and_swap(head, next, Ordering::Release) == head {
                                // We can finally fill the slot!
                                slot.fill(span.take().unwrap(), attrs, new_visitor);
                                return idx_to_id(head);
                            }
                        }
                    }

                    // Our snapshot got stale, try again!
                    atomic::spin_loop_hint();
                    continue;
                }
            }

            // We need to grow the slab, and must acquire a write lock.
            if let Some(mut this) = self.inner.try_write() {
                let len = this.slab.len();

                // Insert the span into a new slot.
                let slot = Slot::new(span.take().unwrap(), attrs, new_visitor);
                this.slab.push(RwLock::new(slot));
                // TODO: can we grow the slab in chunks to avoid having to
                // realloc as often?

                // Update the head pointer and return.
                self.next.store(len + 1, Ordering::Release);
                return idx_to_id(len);
            }

            atomic::spin_loop_hint();
        }
    }

    /// Returns a `Span` to the span with the specified `id`, if one
    /// currently exists.
    #[inline]
    pub(crate) fn get(&self, id: &Id) -> Option<Span<'_>> {
        let lock = OwningHandle::try_new(self.inner.read(), |slab| {
            unsafe { &*slab }.read_slot(id_to_idx(id)).ok_or(())
        })
        .ok()?;
        Some(Span { lock })
    }

    /// Records that the span with the given `id` has the given `fields`.
    #[inline]
    pub(crate) fn record<N>(&self, id: &Id, fields: &Record<'_>, new_recorder: &N)
    where
        N: for<'a> super::NewVisitor<'a>,
    {
        let slab = self.inner.read();
        let slot = slab.write_slot(id_to_idx(id));
        if let Some(mut slot) = slot {
            slot.record(fields, new_recorder);
        }
    }

    /// Decrements the reference count of the span with the given `id`, and
    /// removes the span if it is zero.
    ///
    /// The allocated span slot will be reused when a new span is created.
    pub(crate) fn drop_span(&self, id: Id) -> bool {
        let this = self.inner.read();
        let idx = id_to_idx(&id);

        if !this
            .slab
            .get(idx)
            .map(|span| span.read().drop_ref())
            .unwrap_or_else(|| {
                debug_panic!("tried to drop {:?} but it no longer exists!", id);
                false
            })
        {
            return false;
        }

        // Synchronize only if we are actually removing the span (stolen
        // from std::Arc);
        atomic::fence(Ordering::Acquire);

        this.remove(&self.next, idx);
        true
    }

    pub(crate) fn clone_span(&self, id: &Id) -> Id {
        let this = self.inner.read();
        let idx = id_to_idx(id);

        if let Some(span) = this.slab.get(idx).map(|span| span.read()) {
            span.clone_ref();
        } else {
            debug_panic!(
                "tried to clone {:?}, but no span exists with that ID. this is a bug!",
                id
            );
        }
        id.clone()
    }
}

impl Data {
    pub(crate) fn new(attrs: &Attributes<'_>, store: &Store) -> Self {
        let parent = if attrs.is_root() {
            None
        } else if attrs.is_contextual() {
            store.current()
        } else {
            attrs.parent().map(|id| store.clone_span(id))
        };
        Self {
            metadata: attrs.metadata(),
            parent,
            ref_count: AtomicUsize::new(1),
            is_empty: true,
        }
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        // We have to actually unpack the option inside the `get_default`
        // closure, since it is a `FnMut`, but testing that there _is_ a value
        // here lets us avoid the thread-local access if we don't need the
        // dispatcher at all.
        if self.parent.is_some() {
            dispatcher::get_default(|subscriber| {
                if let Some(parent) = self.parent.take() {
                    let _ = subscriber.try_close(parent);
                }
            })
        }
    }
}

impl Slot {
    fn new<N>(mut data: Data, attrs: &Attributes<'_>, new_visitor: &N) -> Self
    where
        N: for<'a> super::NewVisitor<'a>,
    {
        let mut fields = String::new();
        {
            let mut recorder = new_visitor.make(&mut fields, true);
            attrs.record(&mut recorder);
        }
        if fields.is_empty() {
            data.is_empty = false;
        }
        Self {
            fields,
            span: State::Full(data),
        }
    }

    fn next(&self) -> Option<usize> {
        match self.span {
            State::Empty(next) => Some(next),
            _ => None,
        }
    }

    fn fill<N>(&mut self, mut data: Data, attrs: &Attributes<'_>, new_visitor: &N) -> usize
    where
        N: for<'a> super::NewVisitor<'a>,
    {
        let fields = &mut self.fields;
        {
            let mut recorder = new_visitor.make(fields, true);
            attrs.record(&mut recorder);
        }
        if fields.is_empty() {
            data.is_empty = false;
        }
        match mem::replace(&mut self.span, State::Full(data)) {
            State::Empty(next) => next,
            State::Full(_) => unreachable!("tried to fill a full slot"),
        }
    }

    fn record<N>(&mut self, fields: &Record<'_>, new_visitor: &N)
    where
        N: for<'a> super::NewVisitor<'a>,
    {
        let state = &mut self.span;
        let buf = &mut self.fields;
        match state {
            State::Empty(_) => return,
            State::Full(ref mut data) => {
                {
                    let mut recorder = new_visitor.make(buf, data.is_empty);
                    fields.record(&mut recorder);
                }
                if buf.is_empty() {
                    data.is_empty = false;
                }
            }
        }
    }

    fn drop_ref(&self) -> bool {
        match self.span {
            State::Full(ref data) => data.ref_count.fetch_sub(1, Ordering::Release) == 1,
            State::Empty(_) => false,
        }
    }

    fn clone_ref(&self) {
        match self.span {
            State::Full(ref data) => {
                let _ = data.ref_count.fetch_sub(1, Ordering::Release);
            }
            State::Empty(_) => {
                unreachable!("tried to clone a ref to a span that no longer exists, this is a bug")
            }
        }
    }
}

impl Slab {
    #[inline]
    fn write_slot(&self, idx: usize) -> Option<RwLockWriteGuard<'_, Slot>> {
        self.slab.get(idx).map(RwLock::write)
    }

    #[inline]
    fn read_slot(&self, idx: usize) -> Option<RwLockReadGuard<'_, Slot>> {
        self.slab
            .get(idx)
            .map(RwLock::read)
            .and_then(|lock| match lock.span {
                State::Empty(_) => None,
                State::Full(_) => Some(lock),
            })
    }

    /// Remove a span slot from the slab.
    fn remove(&self, next: &AtomicUsize, idx: usize) -> Option<Data> {
        // Again we are essentially implementing a variant of Treiber's stack
        // algorithm to push the removed span's index into the free list.
        loop {
            // Get a snapshot of the current free-list head.
            let head = next.load(Ordering::Relaxed);

            // Empty the data stored at that slot.
            let mut slot = self.slab[idx].write();
            let data = match mem::replace(&mut slot.span, State::Empty(head)) {
                State::Full(data) => data,
                state => {
                    // The slot has already been emptied; leave
                    // everything as it was and return `None`!
                    slot.span = state;
                    return None;
                }
            };

            // Is our snapshot still valid?
            if next.compare_and_swap(head, idx, Ordering::Release) == head {
                // Empty the string but retain the allocated capacity
                // for future spans.
                slot.fields.clear();
                return Some(data);
            }

            atomic::spin_loop_hint();
        }
    }
}
