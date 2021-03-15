//! Sources and sinks of compressed data
//!
//! # Example
//!
//! The following example encodes and decodes data to and from a file. It uses custom
//! backends that directly write each word to, and read each word from the file. This
//! particular use case of the backend API is not necessarily practical—if you encode all
//! data at once then it's usually simpler to use the default backend, which writes to an
//! in-memory buffer, and then call `.get_compressed()` when you're done to get the buffer.
//! But custom backends similar to the ones used in this example could also be used to add
//! additional processing to the compressed data, such as multiplexing or demultiplexing
//! for some container format.
//!
//! ```
//! use constriction::{
//!     backends::{FallibleCallbackWriteWords, IteratorReadWords},
//!     stream::{
//!         models::DefaultLeakyQuantizer,
//!         queue::{DefaultRangeDecoder, DefaultRangeEncoder},
//!         Decode, Encode,
//!     },
//! };
//! use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
//! use statrs::distribution::Normal;
//! use std::{fs::File, io::{BufReader, BufWriter}};
//!
//! fn encode_to_file_on_the_fly(amt: u32) {
//!     // Some simple entropy model, just for demonstration purpose.
//!     let quantizer = DefaultLeakyQuantizer::new(-256..=255);
//!     let model = quantizer.quantize(Normal::new(0.0, 100.0).unwrap());
//!
//!     // Some long-ish sequence of test symbols, made up in a reproducible way.
//!     let symbols = (0..amt).map(|i| {
//!         let cheap_hash = i.wrapping_mul(0x6979_E2F3).wrapping_add(0x0059_0E91);
//!         (cheap_hash >> (32 - 9)) as i32 - 256
//!     });
//!
//!     // Open a file and build a backend that writes to this file one word at a time.
//!     // (Wrapping the `File` it in a `BufWriter` isn't strictly necessary here,
//!     // it's just good practice when writing to a file.)
//!     let mut file = BufWriter::new(File::create("backend_queue_example.tmp").unwrap());
//!     let write_backend =
//!         FallibleCallbackWriteWords::new(move |word| file.write_u32::<LittleEndian>(word));
//!
//!     // Encapsulate the backend in a `RangeEncoder` and encode (i.e., compress) the symbols.
//!     let mut encoder = DefaultRangeEncoder::with_backend(write_backend);
//!     encoder.encode_iid_symbols(symbols, &model).unwrap();
//!
//!     // Dropping the encoder doesn't automatically seal the compressed bit string because that
//!     // could fail. We explicitly have to seal it by calling `.into_compressed()` (which returns
//!     // the backend since that's what logically "holds" the compressed data.)
//!     std::mem::drop(encoder.into_compressed().unwrap());
//! }
//!
//! fn decode_from_file_on_the_fly(amt: u32) {
//!     // Same toy entropy model that we used for encoding.
//!     let quantizer = DefaultLeakyQuantizer::new(-256..=255);
//!     let model = quantizer.quantize(Normal::new(0.0, 100.0).unwrap());
//!
//!     // Open the file and iterate over its contents in `u32` words (wrapping it in a `BufReader`
//!     // is again just for good practice). We're deliberately being pedantic about the errors
//!     // here in order to show how backend errors can be reported to the encoder.
//!     let mut file = BufReader::new(File::open("backend_queue_example.tmp").unwrap());
//!     let word_iterator = std::iter::from_fn(move || match file.read_u32::<LittleEndian>() {
//!         Ok(word) => Some(Ok(word)),
//!         Err(err) => {
//!             if err.kind() == std::io::ErrorKind::UnexpectedEof {
//!                 None // Reached end of file, end iteration.
//!             } else {
//!                 Some(Err(err)) // Some other I/O error occurred. Propagate it up.
//!             }
//!         }
//!     });
//!
//!     // Create a decoder that decodes on the fly from our iterator.
//!     let mut decoder =
//!         DefaultRangeDecoder::with_backend(IteratorReadWords::new(word_iterator)).unwrap();
//!
//!     // Decode the symbols and verify their correctness.
//!     for (i, symbol) in decoder.decode_iid_symbols(amt as usize, &model).enumerate() {
//!         let cheap_hash = (i as u32).wrapping_mul(0x6979_E2F3).wrapping_add(0x0059_0E91);
//!         let expected = (cheap_hash >> (32 - 9)) as i32 - 256;
//!         assert_eq!(symbol.unwrap(), expected);
//!     }
//!
//!     // Recover the original iterator over compressed words and verify that it's been exhausted.
//!     let mut word_iterator = decoder.into_raw_parts().0.into_iter();
//!     assert!(word_iterator.next().is_none());
//!
//!     // `word_iterator` owns the file since we used a `move` clausure above to construct it.
//!     // So dropping it calls `std::fs::File`'s destructor, which releases the file handle.
//!     std::mem::drop(word_iterator);
//!     std::fs::remove_file("backend_queue_example.tmp").unwrap();
//! }
//!
//! encode_to_file_on_the_fly(1000);
//! decode_from_file_on_the_fly(1000);
//! ```

use alloc::vec::Vec;
use core::{
    convert::Infallible,
    fmt::{Debug, Display},
};
use smallvec::SmallVec;

// READ WRITE LOGICS ==========================================================

pub trait Semantics: Default {}

#[derive(Debug, Default)]
pub struct Stack {}
impl Semantics for Stack {}

#[derive(Debug, Default)]
pub struct Queue {}
impl Semantics for Queue {}

// MAIN TRAITS FOR CAPABILITIES OF BACKENDS ===================================

/// A trait for backends that read compressed words (used by decoders)
///
/// TODO: rename to `ReadWords`, analogous for all other traits in this module.
pub trait ReadWords<Word, S: Semantics> {
    type ReadError: Debug;

    fn read(&mut self) -> Result<Option<Word>, Self::ReadError>;

    fn maybe_exhausted(&self) -> bool {
        true
    }
}

/// A trait for backends that write compressed words (used by encoders)
pub trait WriteWords<Word> {
    type WriteError: Debug;

    fn write(&mut self, word: Word) -> Result<(), Self::WriteError>;

    fn extend_from_iter(
        &mut self,
        iter: impl Iterator<Item = Word>,
    ) -> Result<(), Self::WriteError> {
        for word in iter {
            self.write(word)?;
        }
        Ok(())
    }

    fn maybe_full(&self) -> bool {
        true
    }
}

// A trait for read backends that know how much data is left.
pub trait BoundedReadWords<Word, S: Semantics>: ReadWords<Word, S> {
    // Returns the amount of data that's left for reading.
    fn remaining(&self) -> usize;

    /// TODO: don't forget to overwrite the default implementation of
    /// `ReadWords::maybe_empty`.
    #[inline(always)]
    fn is_exhausted(&self) -> bool {
        self.remaining() == 0
    }
}
// A trait for write backends that know how much more data they're allowed to write.
pub trait BoundedWriteWords<Word>: WriteWords<Word> {
    // Returns the amount of `Word`s that may still be written.
    fn space(&self) -> usize;

    /// TODO: don't forget to overwrite the default implementation of
    /// `WriteWords::maybe_full`.
    #[inline(always)]
    fn is_full(&self) -> bool {
        self.space() == 0
    }
}

// A trait for backends that keep track of their current position in the compressed data.
pub trait PosBackend<Word> {
    fn pos(&self) -> usize;
}

// A trait for backends that allow random access.
pub trait SeekBackend<Word> {
    fn seek(&mut self, pos: usize) -> Result<(), ()>;
}

// TRAITS FOR CONVERSIONS BETWEEN BACKENDS WITH DIFFERENT CAPABILITIES ========

pub trait IntoReadWords<Word, S: Semantics> {
    type IntoReadWords: ReadWords<Word, S>;
    fn into_read_backend(self) -> Self::IntoReadWords;
}

pub trait AsReadWords<'a, Word, S: Semantics>: 'a {
    type AsReadWords: ReadWords<Word, S>;
    fn as_read_backend(&'a self) -> Self::AsReadWords;
}

pub trait IntoSeekReadWords<Word, S: Semantics> {
    type IntoSeekReadWords: SeekBackend<Word> + ReadWords<Word, S>;
    fn into_seek_read_backend(self) -> Self::IntoSeekReadWords;
}

pub trait AsSeekReadWords<'a, Word, S: Semantics>: 'a {
    type AsSeekReadWords: SeekBackend<Word> + ReadWords<Word, S>;
    fn as_seek_read_backend(&'a self) -> Self::AsSeekReadWords;
}

// While neither `SeekBackend` nor `WriteWords` are parameterized by a `ReadWriteLogic`,
// we do need a `ReadWriteLogic` type parameter here because we need to initialize the
// resulting backend correctly.
pub trait IntoSeekWriteWords<Word, S: Semantics> {
    type IntoSeekWriteWords: SeekBackend<Word> + WriteWords<Word>;
    fn into_seek_write_backend(self) -> Self::IntoSeekWriteWords;
}

// While neither `SeekBackend` nor `WriteWords` are parameterized by a `ReadWriteLogic`,
// we do need a `ReadWriteLogic` type parameter here because we need to initialize the
// resulting backend correctly.
pub trait AsSeekWriteWords<'a, Word, S: Semantics>: 'a {
    type AsSeekWriteWords: SeekBackend<Word> + WriteWords<Word>;
    fn as_seek_write_backend(&'a mut self) -> Self::AsSeekWriteWords;
}

// IMPLEMENTATIONS FOR `Vec<Word>` ============================================

impl<Word> WriteWords<Word> for Vec<Word> {
    type WriteError = Infallible;

    #[inline(always)]
    fn write(&mut self, word: Word) -> Result<(), Self::WriteError> {
        self.push(word);
        Ok(())
    }

    fn extend_from_iter(
        &mut self,
        iter: impl Iterator<Item = Word>,
    ) -> Result<(), Self::WriteError> {
        self.extend(iter);
        Ok(())
    }

    fn maybe_full(&self) -> bool {
        false
    }
}

impl<Word> ReadWords<Word, Stack> for Vec<Word> {
    type ReadError = Infallible;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Word>, Self::ReadError> {
        Ok(self.pop())
    }

    #[inline(always)]
    fn maybe_exhausted(&self) -> bool {
        self.is_empty()
    }
}

impl<Word> BoundedReadWords<Word, Stack> for Vec<Word> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.len()
    }
}

impl<Word> PosBackend<Word> for Vec<Word> {
    fn pos(&self) -> usize {
        self.len()
    }
}

// IMPLEMENTATIONS FOR `SmallVec<Word>` =======================================

impl<Array> WriteWords<Array::Item> for SmallVec<Array>
where
    Array: smallvec::Array,
{
    type WriteError = Infallible;

    #[inline(always)]
    fn write(&mut self, word: Array::Item) -> Result<(), Self::WriteError> {
        self.push(word);
        Ok(())
    }

    fn extend_from_iter(
        &mut self,
        iter: impl Iterator<Item = Array::Item>,
    ) -> Result<(), Self::WriteError> {
        self.extend(iter);
        Ok(())
    }

    fn maybe_full(&self) -> bool {
        false
    }
}

impl<Array> ReadWords<Array::Item, Stack> for SmallVec<Array>
where
    Array: smallvec::Array,
{
    type ReadError = Infallible;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Array::Item>, Self::ReadError> {
        Ok(self.pop())
    }

    #[inline(always)]
    fn maybe_exhausted(&self) -> bool {
        self.is_empty()
    }
}

impl<Array> BoundedReadWords<Array::Item, Stack> for SmallVec<Array>
where
    Array: smallvec::Array,
{
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.len()
    }
}

impl<Array> PosBackend<Array::Item> for SmallVec<Array>
where
    Array: smallvec::Array,
{
    fn pos(&self) -> usize {
        self.len()
    }
}

// ADAPTER FOR (SEMANTIC) REVERSING OF READING DIRECTION ======================

#[derive(Debug)]
pub struct ReverseReads<Backend>(pub Backend);

impl<Word, B: WriteWords<Word>> WriteWords<Word> for ReverseReads<B> {
    type WriteError = B::WriteError;

    #[inline(always)]
    fn write(&mut self, word: Word) -> Result<(), Self::WriteError> {
        self.0.write(word)
    }
}

impl<Word, B: ReadWords<Word, Stack>> ReadWords<Word, Queue> for ReverseReads<B> {
    type ReadError = B::ReadError;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Word>, Self::ReadError> {
        self.0.read()
    }

    #[inline(always)]
    fn maybe_exhausted(&self) -> bool {
        self.0.maybe_exhausted()
    }
}

impl<Word, B: ReadWords<Word, Queue>> ReadWords<Word, Stack> for ReverseReads<B> {
    type ReadError = B::ReadError;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Word>, Self::ReadError> {
        self.0.read()
    }

    #[inline(always)]
    fn maybe_exhausted(&self) -> bool {
        self.0.maybe_exhausted()
    }
}

impl<Word, B: BoundedReadWords<Word, Stack>> BoundedReadWords<Word, Queue> for ReverseReads<B> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.0.remaining()
    }

    #[inline(always)]
    fn is_exhausted(&self) -> bool {
        self.0.is_exhausted()
    }
}

impl<Word, B: BoundedReadWords<Word, Queue>> BoundedReadWords<Word, Stack> for ReverseReads<B> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.0.remaining()
    }

    #[inline(always)]
    fn is_exhausted(&self) -> bool {
        self.0.is_exhausted()
    }
}

impl<Word, B: PosBackend<Word>> PosBackend<Word> for ReverseReads<B> {
    #[inline(always)]
    fn pos(&self) -> usize {
        self.0.pos()
    }
}

impl<Word, B: SeekBackend<Word>> SeekBackend<Word> for ReverseReads<B> {
    fn seek(&mut self, pos: usize) -> Result<(), ()> {
        self.0.seek(pos)
    }
}

// ADAPTER FOR IN-MEMORY BUFFERS ==============================================

#[derive(Clone, Debug)]
pub struct Cursor<Buf> {
    buf: Buf,

    /// The index of the next word to be read with a `ReadWords<Word, Queue>` or written
    /// with a `WriteWords<Word>, and one plus the index of the next word to read with
    /// `ReadWords<Word, Stack>.
    ///
    /// Satisfies the invariant `pos <= buf.as_ref().len()` if `Buf: AsRef<[Word]>`.
    pos: usize,
}

impl<Buf> Cursor<Buf> {
    #[inline(always)]
    pub fn new_at_write_beginning(buf: Buf) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline(always)]
    pub fn new_at_write_end<Word>(buf: Buf) -> Self
    where
        Buf: AsRef<[Word]>,
    {
        let pos = buf.as_ref().len();
        Self { buf, pos }
    }

    #[inline(always)]
    pub fn new_at_write_end_mut<Word>(mut buf: Buf) -> Self
    where
        Buf: AsMut<[Word]>,
    {
        let pos = buf.as_mut().len();
        Self { buf, pos }
    }

    pub fn with_buf_and_pos<Word>(buf: Buf, pos: usize) -> Result<Self, ()>
    where
        Buf: AsRef<[Word]>,
    {
        if pos > buf.as_ref().len() {
            Err(())
        } else {
            Ok(Self { buf, pos })
        }
    }

    /// Same as `with_buf_and_pos` except for trait bound. For `Buf`s that implement `AsMut`
    /// but not `AsRef`.
    pub fn with_buf_and_pos_mut<Word>(mut buf: Buf, pos: usize) -> Result<Self, ()>
    where
        Buf: AsMut<[Word]>,
    {
        if pos > buf.as_mut().len() {
            Err(())
        } else {
            Ok(Self { buf, pos })
        }
    }

    pub fn as_view<Word>(&self) -> Cursor<&[Word]>
    where
        Buf: AsRef<[Word]>,
    {
        Cursor {
            buf: self.buf.as_ref(),
            pos: self.pos,
        }
    }

    pub fn as_mut_view<Word>(&mut self) -> Cursor<&mut [Word]>
    where
        Buf: AsMut<[Word]>,
    {
        Cursor {
            buf: self.buf.as_mut(),
            pos: self.pos,
        }
    }

    pub fn cloned<Word: Clone>(&self) -> Cursor<Vec<Word>>
    where
        Buf: AsRef<[Word]>,
    {
        Cursor {
            buf: self.buf.as_ref().to_vec(),
            pos: self.pos,
        }
    }

    pub fn buf(&self) -> &Buf {
        &self.buf
    }

    pub fn into_buf_and_pos(self) -> (Buf, usize) {
        (self.buf, self.pos)
    }

    /// Reverses both the data and the reading direction.
    ///
    /// This method consumes the original `ReadCursor`, reverses the order of the
    /// `Word`s in-place, updates the cursor position accordingly, and returns a
    /// `ReadCursor` that progresses in the opposite direction. Reading from the
    /// returned `ReadCursor` will yield the same `Word`s as continued reading from the
    /// original one would, but the changed direction will be observable via different
    /// behavior of [`Pos::pos`], [`Seek::seek`], and [`Self::buf`].
    pub fn into_reversed<Word>(mut self) -> ReverseReads<Self>
    where
        Buf: AsMut<[Word]>,
    {
        self.buf.as_mut().reverse();
        self.pos = self.buf.as_mut().len() - self.pos;
        ReverseReads(self)
    }
}

impl<Buf> ReverseReads<Cursor<Buf>> {
    pub fn into_reversed<Word>(self) -> Cursor<Buf>
    where
        Buf: AsMut<[Word]>,
    {
        // Accessing `.0` twice removes *two* `ReverseReads`, resulting in no semantic change.
        self.0.into_reversed().0
    }
}

impl<Word, Buf: AsMut<[Word]>> WriteWords<Word> for Cursor<Buf> {
    type WriteError = BoundedWriteError;

    #[inline(always)]
    fn write(&mut self, word: Word) -> Result<(), Self::WriteError> {
        if let Some(target) = self.buf.as_mut().get_mut(self.pos) {
            *target = word;
            self.pos += 1;
            Ok(())
        } else {
            Err(BoundedWriteError::OutOfSpace)
        }
    }
}

impl<Word, Buf: AsMut<[Word]> + AsRef<[Word]>> BoundedWriteWords<Word> for Cursor<Buf> {
    #[inline(always)]
    fn space(&self) -> usize {
        self.buf.as_ref().len() - self.pos
    }
}
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum BoundedWriteError {
    OutOfSpace,
}

impl Display for BoundedWriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfSpace => write!(f, "Out of space."),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BoundedWriteError {}

impl<Word: Clone, Buf: AsRef<[Word]>> ReadWords<Word, Stack> for Cursor<Buf> {
    type ReadError = Infallible;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Word>, Self::ReadError> {
        if self.pos == 0 {
            Ok(None)
        } else {
            self.pos -= 1;
            unsafe {
                // SAFETY: We maintain the invariant `self.pos <= self.buf.as_ref().len()`
                // and we just decreased `self.pos` (and made sure that didn't wrap around),
                // so we now have `self.pos < self.buf.as_ref().len()`.
                Ok(Some(self.buf.as_ref().get_unchecked(self.pos).clone()))
            }
        }
    }

    #[inline(always)]
    fn maybe_exhausted(&self) -> bool {
        BoundedReadWords::<Word, Stack>::is_exhausted(self)
    }
}

impl<Word: Clone, Buf: AsRef<[Word]>> ReadWords<Word, Queue> for Cursor<Buf> {
    type ReadError = Infallible;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Word>, Self::ReadError> {
        let maybe_word = self.buf.as_ref().get(self.pos).cloned();
        if maybe_word.is_some() {
            self.pos += 1;
        }
        Ok(maybe_word)
    }

    #[inline(always)]
    fn maybe_exhausted(&self) -> bool {
        BoundedReadWords::<Word, Queue>::is_exhausted(self)
    }
}

impl<Word: Clone, Buf: AsRef<[Word]>> BoundedReadWords<Word, Stack> for Cursor<Buf> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.pos
    }
}

impl<Word: Clone, Buf: AsRef<[Word]>> BoundedReadWords<Word, Queue> for Cursor<Buf> {
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.buf.as_ref().len() - self.pos
    }
}

impl<Word, Buf: AsRef<[Word]>> PosBackend<Word> for Cursor<Buf> {
    #[inline(always)]
    fn pos(&self) -> usize {
        self.pos
    }
}

impl<Word, Buf: AsRef<[Word]>> SeekBackend<Word> for Cursor<Buf> {
    #[inline(always)]
    fn seek(&mut self, pos: usize) -> Result<(), ()> {
        if pos > self.buf.as_ref().len() {
            // Note that `pos == buf.len()` is still a valid position (EOF for queues and
            // beginning for stacks).
            Err(())
        } else {
            self.pos = pos;
            Ok(())
        }
    }
}

impl<Word: Clone, Buf: AsRef<[Word]>> IntoReadWords<Word, Stack> for Buf {
    type IntoReadWords = Cursor<Buf>;

    fn into_read_backend(self) -> Self::IntoReadWords {
        Cursor::new_at_write_end(self)
    }
}

impl<Word: Clone, Buf: AsRef<[Word]>> IntoReadWords<Word, Queue> for Buf {
    type IntoReadWords = Cursor<Buf>;

    fn into_read_backend(self) -> Self::IntoReadWords {
        Cursor::new_at_write_beginning(self)
    }
}

impl<'a, Word: Clone + 'a, Buf: AsRef<[Word]> + 'a> AsReadWords<'a, Word, Stack> for Buf {
    type AsReadWords = Cursor<&'a [Word]>;

    fn as_read_backend(&'a self) -> Self::AsReadWords {
        Cursor::new_at_write_end(self.as_ref())
    }
}

impl<'a, Word: Clone + 'a, Buf: AsRef<[Word]> + 'a> AsReadWords<'a, Word, Queue> for Buf {
    type AsReadWords = Cursor<&'a [Word]>;

    fn as_read_backend(&'a self) -> Self::AsReadWords {
        Cursor::new_at_write_beginning(self.as_ref())
    }
}

impl<Word, Buf, S: Semantics> IntoSeekReadWords<Word, S> for Buf
where
    Buf: AsRef<[Word]> + IntoReadWords<Word, S, IntoReadWords = Cursor<Buf>>,
    Cursor<Buf>: ReadWords<Word, S>,
{
    type IntoSeekReadWords = Cursor<Buf>;

    fn into_seek_read_backend(self) -> Self::IntoSeekReadWords {
        self.into_read_backend()
    }
}

impl<'a, Word: 'a, Buf, S: Semantics> AsSeekReadWords<'a, Word, S> for Buf
where
    Buf: AsReadWords<'a, Word, S, AsReadWords = Cursor<&'a [Word]>>,
    Cursor<&'a [Word]>: ReadWords<Word, S>,
{
    type AsSeekReadWords = Cursor<&'a [Word]>;

    fn as_seek_read_backend(&'a self) -> Self::AsSeekReadWords {
        self.as_read_backend()
    }
}

impl<Word: Clone, Buf: AsRef<[Word]> + AsMut<[Word]>> IntoSeekWriteWords<Word, Stack> for Buf {
    type IntoSeekWriteWords = Cursor<Buf>;

    fn into_seek_write_backend(self) -> Self::IntoSeekWriteWords {
        Cursor::new_at_write_end_mut(self)
    }
}

impl<Word: Clone, Buf: AsRef<[Word]> + AsMut<[Word]>> IntoSeekWriteWords<Word, Queue> for Buf {
    type IntoSeekWriteWords = Cursor<Buf>;

    fn into_seek_write_backend(self) -> Self::IntoSeekWriteWords {
        Cursor::new_at_write_beginning(self)
    }
}

impl<'a, Word: Clone + 'a, Buf: AsMut<[Word]> + 'a> AsSeekWriteWords<'a, Word, Stack> for Buf {
    type AsSeekWriteWords = Cursor<&'a mut [Word]>;

    fn as_seek_write_backend(&'a mut self) -> Self::AsSeekWriteWords {
        Cursor::new_at_write_end_mut(self.as_mut())
    }
}

impl<'a, Word: Clone + 'a, Buf: AsMut<[Word]> + 'a> AsSeekWriteWords<'a, Word, Queue> for Buf {
    type AsSeekWriteWords = Cursor<&'a mut [Word]>;

    fn as_seek_write_backend(&'a mut self) -> Self::AsSeekWriteWords {
        Cursor::new_at_write_beginning(self.as_mut())
    }
}

// READ ADAPTER FOR ITERATORS =================================================

#[derive(Clone, Debug)]
pub struct IteratorReadWords<Iter: Iterator> {
    inner: core::iter::Fuse<Iter>,
}

impl<Iter: Iterator> IteratorReadWords<Iter> {
    pub fn new(iter: Iter) -> Self {
        Self { inner: iter.fuse() }
    }
}

impl<Iter: Iterator> IntoIterator for IteratorReadWords<Iter> {
    type Item = Iter::Item;
    type IntoIter = core::iter::Fuse<Iter>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner
    }
}

/// Since `IteratorReadWords` doesn't implement `WriteWords`, it is allowed to implement
/// `ReadWords` for all `ReadWriteLogic`s
impl<Iter, S, Word, ReadError> ReadWords<Word, S> for IteratorReadWords<Iter>
where
    Iter: Iterator<Item = Result<Word, ReadError>>,
    S: Semantics,
    ReadError: Debug,
{
    type ReadError = ReadError;

    #[inline(always)]
    fn read(&mut self) -> Result<Option<Word>, Self::ReadError> {
        self.inner.next().transpose()
    }
}

impl<Iter, S, Word, ReadError> BoundedReadWords<Word, S> for IteratorReadWords<Iter>
where
    Iter: ExactSizeIterator<Item = Result<Word, ReadError>>,
    S: Semantics,
    ReadError: Debug,
{
    #[inline(always)]
    fn remaining(&self) -> usize {
        self.inner.len()
    }
}

// WRITE ADAPTER FOR CALLBACKS ================================================

#[derive(Clone, Debug)]
pub struct FallibleCallbackWriteWords<Callback> {
    write_callback: Callback,
}

impl<Callback> FallibleCallbackWriteWords<Callback> {
    pub fn new(write_callback: Callback) -> Self {
        Self { write_callback }
    }

    pub fn into_inner(self) -> Callback {
        self.write_callback
    }
}

impl<Word, WriteError, Callback> WriteWords<Word> for FallibleCallbackWriteWords<Callback>
where
    Callback: FnMut(Word) -> Result<(), WriteError>,
    WriteError: Debug,
{
    type WriteError = WriteError;

    fn write(&mut self, word: Word) -> Result<(), Self::WriteError> {
        (self.write_callback)(word)
    }
}

#[derive(Clone, Debug)]
pub struct InfallibleCallbackWriteWords<Callback> {
    write_callback: Callback,
}

impl<Callback> InfallibleCallbackWriteWords<Callback> {
    pub fn new(write_callback: Callback) -> Self {
        Self { write_callback }
    }

    pub fn into_inner(self) -> Callback {
        self.write_callback
    }
}

impl<Word, Callback> WriteWords<Word> for InfallibleCallbackWriteWords<Callback>
where
    Callback: FnMut(Word),
{
    type WriteError = Infallible;

    fn write(&mut self, word: Word) -> Result<(), Infallible> {
        Ok((self.write_callback)(word))
    }
}

#[cfg(test)]
mod tests {
    use crate::stream::{models::DefaultLeakyQuantizer, stack::DefaultAnsCoder, Decode};
    use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
    use statrs::distribution::Normal;
    use std::{
        fs::File,
        io::{BufReader, BufWriter},
    };

    #[test]
    fn decode_on_the_fly_stack() {
        fn encode_to_file(amt: u32) {
            let quantizer = DefaultLeakyQuantizer::new(-256..=255);
            let model = quantizer.quantize(Normal::new(0.0, 100.0).unwrap());

            let symbols = (0..amt).map(|i| {
                let cheap_hash = i.wrapping_mul(0x6979_E2F3).wrapping_add(0x0059_0E91);
                (cheap_hash >> (32 - 9)) as i32 - 256
            });

            let mut encoder = DefaultAnsCoder::new();
            encoder.encode_iid_symbols_reverse(symbols, &model).unwrap();
            let compressed = encoder.into_compressed().unwrap();

            let mut file = BufWriter::new(File::create("backend_stack_example.tmp").unwrap());
            for &word in compressed.iter().rev() {
                file.write_u32::<LittleEndian>(word).unwrap();
            }
        }

        fn decode_from_file_on_the_fly(amt: u32) {
            let quantizer = DefaultLeakyQuantizer::new(-256..=255);
            let model = quantizer.quantize(Normal::new(0.0, 100.0).unwrap());

            let mut file = BufReader::new(File::open("backend_stack_example.tmp").unwrap());
            let word_iterator = std::iter::from_fn(move || match file.read_u32::<LittleEndian>() {
                Ok(word) => Some(Ok(word)),
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::UnexpectedEof {
                        None
                    } else {
                        Some(Err(err))
                    }
                }
            });

            let mut decoder =
                DefaultAnsCoder::from_reversed_compressed_iter(word_iterator).unwrap();

            for (i, symbol) in decoder.decode_iid_symbols(amt as usize, &model).enumerate() {
                let cheap_hash = (i as u32)
                    .wrapping_mul(0x6979_E2F3)
                    .wrapping_add(0x0059_0E91);
                let expected = (cheap_hash >> (32 - 9)) as i32 - 256;
                assert_eq!(symbol.unwrap(), expected);
            }
            assert!(decoder.is_empty());

            let mut word_iterator = decoder.into_raw_parts().0.into_iter();
            assert!(word_iterator.next().is_none());

            std::mem::drop(word_iterator);
            std::fs::remove_file("backend_stack_example.tmp").unwrap();
        }

        encode_to_file(1000);
        decode_from_file_on_the_fly(1000);
    }
}
