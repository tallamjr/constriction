//! TODO: mirror as much of the `stack` API as possible

#[cfg(feature = "std")]
use std::error::Error;

use alloc::vec::Vec;
use core::{
    borrow::Borrow,
    convert::Infallible,
    fmt::{Debug, Display},
    marker::PhantomData,
    num::NonZeroUsize,
    ops::Deref,
};

use num::cast::AsPrimitive;

use super::{
    backends::{
        AsReadBackend, BoundedReadBackend, Cursor, IntoReadBackend, PosBackend, Queue, ReadBackend,
        SeekBackend, WriteBackend,
    },
    models::{DecoderModel, EncoderModel},
    Code, Decode, Encode, IntoDecoder, Pos, Seek,
};
use crate::{BitArray, CoderError, EncoderError, EncoderFrontendError, UnwrapInfallible};

/// Type of the internal state used by [`Encoder<CompressedWord, State>`],
/// [`Decoder<CompressedWord, State>`]. Relevant for [`Seek`]ing.
///
/// [`Seek`]: crate::Seek
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoderState<CompressedWord, State> {
    lower: State,

    /// Invariant: `range >= State::one() << (State::BITS - CompressedWord::BITS)`
    /// Therefore, the highest order `CompressedWord` of `lower` is always sufficient to
    /// identify the current interval, so only it has to be flushed at the end.
    range: State,

    /// We keep track of the `CompressedWord` type so that we can statically enforce
    /// the invariants for `lower` and `range`.
    phantom: PhantomData<CompressedWord>,
}

impl<CompressedWord, State: BitArray> CoderState<CompressedWord, State> {
    /// Get the lower bound of the current range (inclusive)
    pub fn lower(&self) -> State {
        self.lower
    }

    /// Get the size of the current range
    pub fn range(&self) -> State {
        self.range
    }
}

impl<CompressedWord: BitArray, State: BitArray> Default for CoderState<CompressedWord, State> {
    fn default() -> Self {
        Self {
            lower: State::zero(),
            range: State::max_value(),
            phantom: PhantomData,
        }
    }
}

pub struct RangeEncoder<CompressedWord, State, Backend = Vec<CompressedWord>>
where
    CompressedWord: BitArray,
    State: BitArray,
    Backend: WriteBackend<CompressedWord>,
{
    bulk: Backend,
    state: CoderState<CompressedWord, State>,
    situation: EncoderSituation<CompressedWord>,
}

#[derive(Debug, PartialEq, Eq)]
enum EncoderSituation<CompressedWord> {
    Normal,

    /// Wraps `num_inverted` and `first_inverted_lower_word`
    Inverted(NonZeroUsize, CompressedWord),
}

impl<CompressedWord> Default for EncoderSituation<CompressedWord> {
    fn default() -> Self {
        Self::Normal
    }
}

/// Type alias for an [`RangeEncoder`] with sane parameters for typical use cases.
pub type DefaultRangeEncoder<Backend = Vec<u32>> = RangeEncoder<u32, u64, Backend>;

/// Type alias for a [`RangeEncoder`] for use with [lookup models]
///
/// This encoder has a smaller word size and internal state than [`DefaultRangeEncoder`]. It
/// is optimized for use with lookup entropy models, in particular with a
/// [`DefaultEncoderArrayLookupTable`] or a [`DefaultEncoderHashLookupTable`].
///
/// # Examples
///
/// See [`DefaultEncoderArrayLookupTable`] and [`DefaultEncoderHashLookupTable`].
///
/// # See also
///
/// - [`SmallRangeDecoder`]
///
/// [lookup models]: super::models::lookup
/// [`DefaultEncoderArrayLookupTable`]: super::models::lookup::DefaultEncoderArrayLookupTable
/// [`DefaultEncoderHashLookupTable`]: super::models::lookup::DefaultEncoderHashLookupTable
pub type SmallRangeEncoder<Backend = Vec<u16>> = RangeEncoder<u16, u32, Backend>;

impl<CompressedWord, State, Backend> Debug for RangeEncoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: WriteBackend<CompressedWord>,
    for<'a> &'a Backend: IntoIterator<Item = &'a CompressedWord>,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter_compressed()).finish()
    }
}

impl<CompressedWord, State, Backend> Code for RangeEncoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: WriteBackend<CompressedWord>,
{
    type State = CoderState<CompressedWord, State>;
    type CompressedWord = CompressedWord;

    fn state(&self) -> Self::State {
        self.state
    }
}

impl<CompressedWord, State> Pos for RangeEncoder<CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    fn pos(&self) -> usize {
        let num_inverted = if let EncoderSituation::Inverted(num_inverted, _) = self.situation {
            num_inverted.get()
        } else {
            0
        };
        self.bulk.len() + num_inverted
    }
}

impl<CompressedWord, State> Default for RangeEncoder<CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<CompressedWord, State> RangeEncoder<CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    /// Creates an empty encoder for range coding.
    pub fn new() -> Self {
        assert!(State::BITS >= 2 * CompressedWord::BITS);
        assert_eq!(State::BITS % CompressedWord::BITS, 0);

        Self {
            bulk: Vec::new(),
            state: CoderState::default(),
            situation: EncoderSituation::Normal,
        }
    }
}

impl<CompressedWord, State, Backend> RangeEncoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: WriteBackend<CompressedWord>,
{
    /// Check if no data has been encoded yet.
    pub fn is_empty<'a>(&'a self) -> bool
    where
        Backend: AsReadBackend<'a, CompressedWord, Queue>,
        Backend::AsReadBackend: BoundedReadBackend<CompressedWord, Queue>,
    {
        self.state.range == State::max_value() && self.bulk.as_read_backend().is_exhausted()
    }

    /// Same as IntoDecoder::into_decoder(self) but can be used for any `PRECISION`
    /// and therefore doesn't require type arguments on the caller side.
    ///
    /// TODO: there should also be a `decoder()` method that takes `&mut self`
    pub fn into_decoder(
        self,
    ) -> Result<RangeDecoder<CompressedWord, State, Backend::IntoReadBackend>, ()>
    where
        Backend: IntoReadBackend<CompressedWord, Queue>,
    {
        // TODO: return proper error (or just box it up).
        RangeDecoder::from_compressed(self.into_compressed().map_err(|_| ())?).map_err(|_| ())
    }

    pub fn into_compressed(mut self) -> Result<Backend, Backend::WriteError> {
        self.seal()?;
        Ok(self.bulk)
    }

    /// Private method; flushes held-back words if in inverted situation and adds a single
    /// additional word that identifies the range (unless no symbols have been encoded yet,
    /// in which case this is a no-op).
    ///
    /// Doesn't change `self.state` or `self.situation` so that this operation can be
    /// reversed if the backend supports removing words (see method `unseal`);
    fn seal(&mut self) -> Result<(), Backend::WriteError> {
        if self.state.range == State::max_value() {
            // This condition only holds upon initialization because encoding a symbol first
            // reduces `range` and then only (possibly) right-shifts it, which introduces
            // some zero bits. We treat this case special and don't emit any words, so that
            // an empty sequence of symbols gets encoded to an empty sequence of words.
            return Ok(());
        }

        let point = self
            .state
            .lower
            .wrapping_add(&(self.state.range - State::one()));

        if let EncoderSituation::Inverted(num_inverted, first_inverted_lower_word) = self.situation
        {
            let (first_word, consecutive_words) = if point < self.state.lower {
                (
                    first_inverted_lower_word + CompressedWord::one(),
                    CompressedWord::zero(),
                )
            } else {
                (first_inverted_lower_word, CompressedWord::max_value())
            };

            self.bulk.write(first_word)?;
            for _ in 1..num_inverted.get() {
                self.bulk.write(consecutive_words)?;
            }
        }

        let word = (point >> (State::BITS - CompressedWord::BITS)).as_();
        self.bulk.write(word)?;

        Ok(())
    }

    /// TODO: this is out of date
    pub fn iter_compressed<'a>(&'a self) -> impl Iterator<Item = CompressedWord> + '_
    where
        &'a Backend: IntoIterator<Item = &'a CompressedWord>,
    {
        let bulk_iter = self.bulk.into_iter().cloned();
        let last = (self.state.lower >> (State::BITS - CompressedWord::BITS)).as_();
        let state_iter = core::iter::once(last);
        bulk_iter.chain(state_iter)
    }

    /// Returns the number of compressed words on the ans.
    ///
    /// This includes a constant overhead of between one and two words unless the
    /// coder is completely empty.
    ///
    /// This method returns the length of the slice, the `Vec<CompressedWord>`, or the iterator
    /// that would be returned by [`get_compressed`], [`into_compressed`], or
    /// [`iter_compressed`], respectively, when called at this time.
    ///
    /// See also [`num_bits`].
    ///
    /// [`get_compressed`]: #method.get_compressed
    /// [`into_compressed`]: #method.into_compressed
    /// [`iter_compressed`]: #method.iter_compressed
    /// [`num_bits`]: #method.num_bits
    pub fn num_words<'a>(&'a self) -> usize
    where
        Backend: AsReadBackend<'a, CompressedWord, Queue>,
        Backend::AsReadBackend: BoundedReadBackend<CompressedWord, Queue>,
    {
        if self.is_empty() {
            0
        } else {
            self.bulk.as_read_backend().remaining() + 1
        }
    }

    /// Returns the size of the current queue of compressed data in bits.
    ///
    /// This includes some constant overhead unless the coder is completely empty
    /// (see [`num_words`](#method.num_words)).
    ///
    /// The returned value is a multiple of the bitlength of the compressed word
    /// type `CompressedWord`.
    pub fn num_bits<'a>(&'a self) -> usize
    where
        Backend: AsReadBackend<'a, CompressedWord, Queue>,
        Backend::AsReadBackend: BoundedReadBackend<CompressedWord, Queue>,
    {
        CompressedWord::BITS * self.num_words()
    }

    pub fn bulk(&self) -> &Backend {
        &self.bulk
    }
}

impl<CompressedWord, State> RangeEncoder<CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    /// Discards all compressed data and resets the coder to the same state as
    /// [`Coder::new`](#method.new).
    pub fn clear(&mut self) {
        self.bulk.clear();
        self.state = CoderState::default();
    }

    /// Assembles the current compressed data into a single slice.
    ///
    /// This method is only implemented for encoders backed by a `Vec<CompressedWord>`
    /// because we have to temporarily seal the encoder and then unseal it when the returned
    /// `EncoderGuard` is dropped, which requires precise knowledge of the backend (and
    /// which is also the reason why this method takes a `&mut self`receiver). If you're
    /// using a different backend than a `Vec`, consider calling [`into_compressed`]
    /// instead.
    ///
    /// TODO: update following documentation.
    ///
    /// This method is similar to [`as_compressed_raw`] with the difference that it
    /// concatenates the `bulk` and `head` before returning them. The concatenation
    /// truncates any trailing zero words, which is compatible with the constructor
    /// [`from_compressed`].
    ///
    /// This method requires a `&mut self` receiver. If you only have a shared reference to
    /// a `Coder`, consider calling [`as_compressed_raw`] or [`iter_compressed`] instead.
    ///
    /// The returned `CoderGuard` dereferences to `&[CompressedWord]`, thus providing
    /// read-only access to the compressed data. If you need ownership of the compressed
    /// data, consider calling [`into_compressed`] instead.
    ///
    /// # Example
    ///
    /// ```
    /// use constriction::stream::{models::Categorical, stack::DefaultAnsCoder, Decode};
    ///
    /// let mut coder = DefaultAnsCoder::new();
    ///
    /// // Push some data on the coder.
    /// let symbols = vec![8, 2, 0, 7];
    /// let probabilities = vec![0.03, 0.07, 0.1, 0.1, 0.2, 0.2, 0.1, 0.15, 0.05];
    /// let model = Categorical::<u32, 24>::from_floating_point_probabilities(&probabilities)
    ///     .unwrap();
    /// coder.encode_iid_symbols_reverse(&symbols, &model).unwrap();
    ///
    /// // Inspect the compressed data.
    /// dbg!(coder.get_compressed());
    ///
    /// // We can still use the coder afterwards.
    /// let reconstructed = coder
    ///     .decode_iid_symbols(4, &model)
    ///     .collect::<Result<Vec<_>, _>>()
    ///     .unwrap();
    /// assert_eq!(reconstructed, symbols);
    /// ```
    ///
    /// TODO: this is currently out of date
    ///
    /// [`as_compressed_raw`]: #method.as_compressed_raw [`from_compressed`]:
    /// #method.from_compressed [`iter_compressed`]: #method.iter_compressed
    /// [`into_compressed`]: #method.into_compressed
    pub fn get_compressed(&mut self) -> EncoderGuard<'_, CompressedWord, State> {
        EncoderGuard::new(self)
    }

    /// A decoder for temporary use.
    ///
    /// Once the returned decoder gets dropped, you can continue using this encoder. If you
    /// don't need this flexibility, call [`into_decoder`] instead.
    ///
    /// This method is only implemented for encoders backed by a `Vec<CompressedWord>`
    /// because we have to temporarily seal the encoder and then unseal it when the returned
    /// decoder is dropped, which requires precise knowledge of the backend (and which is
    /// also the reason why this method takes a `&mut self`receiver). If you're using a
    /// different backend than a `Vec`, consider calling [`into_decoder`] instead.
    pub fn decoder<'a>(
        &'a mut self,
    ) -> RangeDecoder<CompressedWord, State, Cursor<EncoderGuard<'a, CompressedWord, State>>> {
        RangeDecoder::from_compressed(self.get_compressed()).unwrap_infallible()
    }

    fn unseal(&mut self) {
        if self.bulk.is_empty() {
            return;
        }

        self.bulk.pop();

        if let EncoderSituation::Inverted(num_inverted, _) = self.situation {
            for _ in 0..num_inverted.get() {
                self.bulk.pop();
            }
        }
    }
}

impl<CompressedWord, State, Backend, const PRECISION: usize> IntoDecoder<PRECISION>
    for RangeEncoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: WriteBackend<CompressedWord> + IntoReadBackend<CompressedWord, Queue>,
{
    type IntoDecoder = RangeDecoder<CompressedWord, State, Backend::IntoReadBackend>;
}

impl<CompressedWord, State, const PRECISION: usize> Encode<PRECISION>
    for RangeEncoder<CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    type BackendError = Infallible;

    fn encode_symbol<D>(
        &mut self,
        symbol: impl Borrow<D::Symbol>,
        model: D,
    ) -> Result<(), EncoderError<Self::BackendError>>
    where
        D: EncoderModel<PRECISION>,
        D::Probability: Into<Self::CompressedWord>,
        Self::CompressedWord: AsPrimitive<D::Probability>,
    {
        // We maintain the following invariant (*):
        //   range >= State::one() << (State::BITS - CompressedWord::BITS)

        let (left_sided_cumulative, probability) = model
            .left_cumulative_and_probability(symbol)
            .map_err(|()| EncoderFrontendError::ImpossibleSymbol.into_encoder_error())?;

        let scale = self.state.range >> PRECISION;
        // This cannot overflow since `scale * probability <= (range >> PRECISION) << PRECISION`
        self.state.range = scale * probability.into().into();
        let new_lower = self
            .state
            .lower
            .wrapping_add(&(scale * left_sided_cumulative.into().into()));

        // TODO: mark as unlikely branch.
        if let EncoderSituation::Inverted(num_inverted, first_inverted_lower_word) = self.situation
        {
            if new_lower.wrapping_add(&self.state.range) > new_lower {
                // We've transitioned from an inverted to a normal situation.

                let (first_word, consecutive_words) = if new_lower < self.state.lower {
                    (
                        first_inverted_lower_word + CompressedWord::one(),
                        CompressedWord::zero(),
                    )
                } else {
                    (first_inverted_lower_word, CompressedWord::max_value())
                };

                self.bulk.write(first_word)?;
                for _ in 1..num_inverted.get() {
                    self.bulk.write(consecutive_words)?;
                }

                self.situation = EncoderSituation::Normal;
            }
        }

        self.state.lower = new_lower;

        if self.state.range < State::one() << (State::BITS - CompressedWord::BITS) {
            // Invariant `range >= State::one() << (State::BITS - CompressedWord::BITS)` is
            // violated. Since `left_cumulative_and_probability` succeeded, we know that
            // `probability != 0` and therefore:
            //   range >= scale * probability = (old_range >> PRECISION) * probability
            //         >= old_range >> PRECISION
            //         >= old_range >> CompressedWords::BITS
            // where `old_range` is the `range` at method entry, which satisfied invariant (*)
            // by assumption. Therefore, the following left-shift restores the invariant:
            self.state.range = self.state.range << CompressedWord::BITS;

            let lower_word = (self.state.lower >> (State::BITS - CompressedWord::BITS)).as_();
            self.state.lower = self.state.lower << CompressedWord::BITS;

            if let EncoderSituation::Inverted(num_inverted, _) = &mut self.situation {
                // Transition from an inverted to an inverted situation (TODO: mark as unlikely branch).
                *num_inverted = NonZeroUsize::new(num_inverted.get().wrapping_add(1))
                    .expect("Cannot encode more symbols than what's addressable with usize.");
            } else {
                if self.state.lower.wrapping_add(&self.state.range) > self.state.lower {
                    // Transition from a normal to a normal situation (the most common case).
                    self.bulk.write(lower_word)?;
                } else {
                    // Transition from a normal to an inverted situation.
                    self.situation = EncoderSituation::Inverted(
                        NonZeroUsize::new(1).expect("1 != 0"),
                        lower_word,
                    );
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct RangeDecoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray,
    State: BitArray,
    Backend: ReadBackend<CompressedWord, Queue>,
{
    bulk: Backend,

    state: CoderState<CompressedWord, State>,

    /// Invariant: `point.wrapping_sub(&state.lower) < state.range`
    point: State,
}

/// Type alias for a [`RangeDecoder`] with sane parameters for typical use cases.
pub type DefaultRangeDecoder<Backend = Vec<u32>> = RangeDecoder<u32, u64, Backend>;

/// Type alias for a [`RangeDecoder`] for use with [lookup models]
///
/// This encoder has a smaller word size and internal state than [`DefaultRangeDecoder`]. It
/// is optimized for use with lookup entropy models, in particular with a
/// [`DefaultDecoderIndexLookupTable`] or a [`DefaultDecoderGenericLookupTable`].
///
/// # Examples
///
/// See [`DefaultDecoderIndexLookupTable`] and [`DefaultDecoderGenericLookupTable`].
///
/// # See also
///
/// - [`SmallRangeEncoder`]
///
/// [lookup models]: super::models::lookup
/// [`DefaultEncoderArrayLookupTable`]: super::models::lookup::DefaultEncoderArrayLookupTable
/// [`DefaultEncoderHashLookupTable`]: super::models::lookup::DefaultEncoderHashLookupTable
/// [`DefaultDecoderIndexLookupTable`]: super::models::lookup::DefaultDecoderIndexLookupTable
/// [`DefaultDecoderGenericLookupTable`]: super::models::lookup::DefaultDecoderGenericLookupTable
pub type SmallRangeDecoder<Backend> = RangeDecoder<u16, u32, Backend>;

// TODO: uncomment
// impl<CompressedWord, State, Backend> Debug for RangeDecoder<CompressedWord, State, Backend>
// where
//     CompressedWord: BitArray + Into<State>,
//     State: BitArray + AsPrimitive<CompressedWord>,
//     Backend: ReadBackend<CompressedWord,Queue>,
//     for<'a> &'a Backend: IntoIterator<Item = &'a CompressedWord>,
// {
//     fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
//         f.debug_list()
//             .entries(
//                 bit_array_to_chunks_exact(self.state.lower)
//                     .chain(self.bulk.as_ref().iter().cloned()),
//             )
//             .finish()
//     }
// }

impl<CompressedWord, State, Backend> RangeDecoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: ReadBackend<CompressedWord, Queue>,
{
    pub fn from_compressed<Buf>(compressed: Buf) -> Result<Self, Backend::ReadError>
    where
        Buf: IntoReadBackend<CompressedWord, Queue, IntoReadBackend = Backend>,
    {
        assert!(State::BITS >= 2 * CompressedWord::BITS);
        assert_eq!(State::BITS % CompressedWord::BITS, 0);

        let mut bulk = compressed.into_read_backend();
        let point = Self::read_point(&mut bulk)?;

        Ok(RangeDecoder {
            bulk,
            state: CoderState::default(),
            point,
        })
    }

    pub fn for_compressed<'a, Buf>(compressed: &'a Buf) -> Result<Self, Backend::ReadError>
    where
        Buf: AsReadBackend<'a, CompressedWord, Queue, AsReadBackend = Backend>,
    {
        assert!(State::BITS >= 2 * CompressedWord::BITS);
        assert_eq!(State::BITS % CompressedWord::BITS, 0);

        let mut bulk = compressed.as_read_backend();
        let point = Self::read_point(&mut bulk)?;

        Ok(RangeDecoder {
            bulk,
            state: CoderState::default(),
            point,
        })
    }

    pub fn from_raw_parts(
        _bulk: Backend,
        _state: State,
    ) -> Result<Self, (Backend, CoderState<CompressedWord, State>)> {
        assert!(State::BITS >= 2 * CompressedWord::BITS);
        assert_eq!(State::BITS % CompressedWord::BITS, 0);

        todo!()
    }

    pub fn into_raw_parts(self) -> (Backend, CoderState<CompressedWord, State>) {
        (self.bulk, self.state)
    }

    fn read_point<B: ReadBackend<CompressedWord, Queue>>(
        bulk: &mut B,
    ) -> Result<State, B::ReadError> {
        let mut num_read = 0;
        let mut point = State::zero();
        while let Some(word) = bulk.read()? {
            point = point << CompressedWord::BITS | word.into();
            num_read += 1;
            if num_read == State::BITS / CompressedWord::BITS {
                break;
            }
        }

        if num_read < State::BITS / CompressedWord::BITS {
            if num_read != 0 {
                point = point << (State::BITS - num_read * CompressedWord::BITS);
            }
            // TODO: do we need to advance the Backend's `pos` beyond the end to make
            // `PosBackend` consistent with its implementation for the encoder?
        }

        Ok(point)
    }
}

impl<CompressedWord, State, Backend> Code for RangeDecoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: ReadBackend<CompressedWord, Queue>,
{
    type State = CoderState<CompressedWord, State>;
    type CompressedWord = CompressedWord;

    fn state(&self) -> Self::State {
        self.state
    }

    fn maybe_empty(&self) -> bool {
        // The check for `self.state.range == State::max_value()` is for the special case of
        // an empty buffer.
        self.bulk.maybe_exhausted()
            && (self.state.range == State::max_value()
                || self
                    .state
                    .lower
                    .wrapping_add(&self.state.range)
                    .wrapping_sub(&self.point)
                    <= State::one() << (State::BITS - CompressedWord::BITS))
    }
}

impl<CompressedWord, State, Backend> Pos for RangeDecoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: ReadBackend<CompressedWord, Queue> + PosBackend<CompressedWord>,
{
    fn pos(&self) -> usize {
        self.bulk
            .pos()
            .saturating_sub(State::BITS / CompressedWord::BITS)
    }
}

impl<CompressedWord, State, Backend> Seek for RangeDecoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: ReadBackend<CompressedWord, Queue> + SeekBackend<CompressedWord>,
{
    fn seek(&mut self, pos_and_state: (usize, Self::State)) -> Result<(), ()> {
        let (pos, state) = pos_and_state;

        self.bulk.seek(pos)?;
        self.point = Self::read_point(&mut self.bulk).map_err(|_| ())?;
        self.state = state;

        // TODO: deal with positions very close to end.

        Ok(())
    }
}

impl<CompressedWord, State, Backend> From<RangeEncoder<CompressedWord, State, Backend>>
    for RangeDecoder<CompressedWord, State, Backend::IntoReadBackend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: WriteBackend<CompressedWord> + IntoReadBackend<CompressedWord, Queue>,
{
    fn from(encoder: RangeEncoder<CompressedWord, State, Backend>) -> Self {
        // TODO: implement a `try_into_decoder` or something instead. Or specialize this
        // method to the case where both read and write error are Infallible, which is
        // probably the only place where this will be used anyway.
        encoder.into_decoder().unwrap()
    }
}

// TODO
// impl<'a, CompressedWord, State, Backend> From<&'a RangeEncoder<CompressedWord, State, Backend>>
//     for RangeDecoder<CompressedWord, State, Backend::AsReadBackend>
// where
//     CompressedWord: BitArray + Into<State>,
//     State: BitArray + AsPrimitive<CompressedWord>,
//     Backend: WriteBackend<CompressedWord> + AsReadBackend<'a, CompressedWord, Queue>,
// {
//     fn from(encoder: &'a RangeEncoder<CompressedWord, State, Backend>) -> Self {
//         encoder.as_decoder()
//     }
// }

impl<CompressedWord, State, Backend, const PRECISION: usize> Decode<PRECISION>
    for RangeDecoder<CompressedWord, State, Backend>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
    Backend: ReadBackend<CompressedWord, Queue>,
{
    type FrontendError = FrontendError;

    type BackendError = Backend::ReadError;

    /// Decodes a single symbol and pops it off the compressed data.
    ///
    /// This is a low level method. You usually probably want to call a batch method
    /// like [`decode_symbols`](#method.decode_symbols) or
    /// [`decode_iid_symbols`](#method.decode_iid_symbols) instead.
    ///
    /// This method is called `decode_symbol` rather than `decode_symbol` to stress the
    /// fact that the `Coder` is a stack: `decode_symbol` will return the *last* symbol
    /// that was previously encoded via [`encode_symbol`](#method.encode_symbol).
    ///
    /// Note that this method cannot fail. It will still produce symbols in a
    /// deterministic way even if the coder is empty, but such symbols will not
    /// recover any previously encoded data and will generally have low entropy.
    /// Still, being able to pop off an arbitrary number of symbols can sometimes be
    /// useful in edge cases of, e.g., the bits-back algorithm.
    fn decode_symbol<D>(
        &mut self,
        model: D,
    ) -> Result<D::Symbol, CoderError<Self::FrontendError, Self::BackendError>>
    where
        D: DecoderModel<PRECISION>,
        D::Probability: Into<Self::CompressedWord>,
        Self::CompressedWord: AsPrimitive<D::Probability>,
    {
        // We maintain the following invariant (*):
        //   point (-) lower < range
        // where (-) denotes wrapping subtraction (in `Self::State`).

        let scale = self.state.range >> PRECISION;
        let quantile = self.point.wrapping_sub(&self.state.lower) / scale;
        if quantile >= State::one() << PRECISION {
            // This can only happen if both of the following conditions apply:
            // (i) we are decoding invalid compressed data; and
            // (ii) we use entropy models with varying `PRECISION`s.
            // TODO: Is (ii) necessary? Aren't there always unreachable pockets due to rounding?
            return Err(CoderError::FrontendError(FrontendError::InvalidData));
        }

        let (symbol, left_sided_cumulative, probability) =
            model.quantile_function(quantile.as_().as_());

        // Update `state` in the same way as we do in `encode_symbol` (see comments there):
        self.state.lower = self
            .state
            .lower
            .wrapping_add(&(scale * left_sided_cumulative.into().into()));
        self.state.range = scale * probability.into().into();

        // Invariant (*) is still satisfied at this point because:
        //   (point (-) lower) / scale = (point (-) old_lower) / scale (-) left_sided_cumulative
        //                             = quantile (-) left_sided_cumulative
        //                             < probability
        // Therefore, we have:
        //   point (-) lower < scale * probability <= range

        if self.state.range < State::one() << (State::BITS - CompressedWord::BITS) {
            // First update `state` in the same way as we do in `encode_symbol`:
            self.state.lower = self.state.lower << CompressedWord::BITS;
            self.state.range = self.state.range << CompressedWord::BITS;

            // Then update `point`, which restores invariant (*):
            self.point = self.point << CompressedWord::BITS;
            if let Some(word) = self.bulk.read()? {
                self.point = self.point | word.into();
            }

            // TODO: register reads past end.
        }

        Ok(symbol)
    }
}

/// Provides temporary read-only access to the compressed data wrapped in an
/// [`Encoder`].
///
/// Dereferences to `&[CompressedWord]`. See [`Encoder::get_compressed`] for an example.
///
/// [`Coder`]: struct.Coder.html
/// [`Coder::get_compressed`]: struct.Coder.html#method.get_compressed
pub struct EncoderGuard<'a, CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    inner: &'a mut RangeEncoder<CompressedWord, State>,
}

impl<CompressedWord, State> Debug for EncoderGuard<'_, CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<'a, CompressedWord, State> EncoderGuard<'a, CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    fn new(encoder: &'a mut RangeEncoder<CompressedWord, State>) -> Self {
        // Append state. Will be undone in `<Self as Drop>::drop`.
        if !encoder.is_empty() {
            encoder.seal().unwrap_infallible();
        }
        Self { inner: encoder }
    }
}

impl<'a, CompressedWord, State> Drop for EncoderGuard<'a, CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    fn drop(&mut self) {
        self.inner.unseal();
    }
}

impl<'a, CompressedWord, State> Deref for EncoderGuard<'a, CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    type Target = [CompressedWord];

    fn deref(&self) -> &Self::Target {
        &self.inner.bulk
    }
}

impl<'a, CompressedWord, State> AsRef<[CompressedWord]> for EncoderGuard<'a, CompressedWord, State>
where
    CompressedWord: BitArray + Into<State>,
    State: BitArray + AsPrimitive<CompressedWord>,
{
    fn as_ref(&self) -> &[CompressedWord] {
        self
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::dbg;

    use super::super::models::{Categorical, LeakyQuantizer};
    use super::*;

    use rand_xoshiro::{
        rand_core::{RngCore, SeedableRng},
        Xoshiro256StarStar,
    };
    use statrs::distribution::{InverseCDF, Normal};

    #[test]
    fn compress_none() {
        let encoder = DefaultRangeEncoder::new();
        assert!(encoder.is_empty());
        let compressed = encoder.into_compressed().unwrap();
        assert!(compressed.is_empty());

        let decoder = DefaultRangeDecoder::from_compressed(compressed).unwrap();
        assert!(decoder.maybe_empty());
    }

    #[test]
    fn compress_one() {
        generic_compress_few(core::iter::once(5), 1)
    }

    #[test]
    fn compress_two() {
        generic_compress_few([2, 8].iter().cloned(), 1)
    }

    #[test]
    fn compress_ten() {
        generic_compress_few(0..10, 2)
    }

    #[test]
    fn compress_twenty() {
        generic_compress_few(-10..10, 4)
    }

    fn generic_compress_few<I>(symbols: I, expected_size: usize)
    where
        I: IntoIterator<Item = i32>,
        I::IntoIter: Clone,
    {
        let symbols = symbols.into_iter();

        let mut encoder = DefaultRangeEncoder::new();
        let quantizer = LeakyQuantizer::<_, _, u32, 24>::new(-127..=127);
        let model = quantizer.quantize(Normal::new(3.2, 5.1).unwrap());

        encoder.encode_iid_symbols(symbols.clone(), &model).unwrap();
        let compressed = encoder.into_compressed().unwrap();
        assert_eq!(compressed.len(), expected_size);

        let mut decoder = DefaultRangeDecoder::from_compressed(&compressed).unwrap();
        for symbol in symbols {
            assert_eq!(decoder.decode_symbol(&model).unwrap(), symbol);
        }
        assert!(decoder.maybe_empty());
    }

    #[test]
    fn compress_many_u32_u64_32() {
        generic_compress_many::<u32, u64, u32, 32>();
    }

    #[test]
    fn compress_many_u32_u64_24() {
        generic_compress_many::<u32, u64, u32, 24>();
    }

    #[test]
    fn compress_many_u32_u64_16() {
        generic_compress_many::<u32, u64, u16, 16>();
    }

    #[test]
    fn compress_many_u32_u64_8() {
        generic_compress_many::<u32, u64, u8, 8>();
    }

    #[test]
    fn compress_many_u16_u64_16() {
        generic_compress_many::<u16, u64, u16, 16>();
    }

    #[test]
    fn compress_many_u16_u64_12() {
        generic_compress_many::<u16, u64, u16, 12>();
    }

    #[test]
    fn compress_many_u16_u64_8() {
        generic_compress_many::<u16, u64, u8, 8>();
    }

    #[test]
    fn compress_many_u8_u64_8() {
        generic_compress_many::<u8, u64, u8, 8>();
    }

    #[test]
    fn compress_many_u16_u32_16() {
        generic_compress_many::<u16, u32, u16, 16>();
    }

    #[test]
    fn compress_many_u16_u32_12() {
        generic_compress_many::<u16, u32, u16, 12>();
    }

    #[test]
    fn compress_many_u16_u32_8() {
        generic_compress_many::<u16, u32, u8, 8>();
    }

    #[test]
    fn compress_many_u8_u32_8() {
        generic_compress_many::<u8, u32, u8, 8>();
    }

    #[test]
    fn compress_many_u8_u16_8() {
        generic_compress_many::<u8, u16, u8, 8>();
    }

    fn generic_compress_many<CompressedWord, State, Probability, const PRECISION: usize>()
    where
        State: BitArray + AsPrimitive<CompressedWord>,
        CompressedWord: BitArray + Into<State> + AsPrimitive<Probability>,
        Probability: BitArray + Into<CompressedWord> + AsPrimitive<usize> + Into<f64>,
        u32: AsPrimitive<Probability>,
        usize: AsPrimitive<Probability>,
        f64: AsPrimitive<Probability>,
        i32: AsPrimitive<Probability>,
    {
        const AMT: usize = 1000;
        let mut symbols_gaussian = Vec::with_capacity(AMT);
        let mut means = Vec::with_capacity(AMT);
        let mut stds = Vec::with_capacity(AMT);

        let mut rng = Xoshiro256StarStar::seed_from_u64(1234);
        for _ in 0..AMT {
            let mean = (200.0 / u32::MAX as f64) * rng.next_u32() as f64 - 100.0;
            let std_dev = (10.0 / u32::MAX as f64) * rng.next_u32() as f64 + 0.001;
            let quantile = (rng.next_u32() as f64 + 0.5) / (1u64 << 32) as f64;
            let dist = Normal::new(mean, std_dev).unwrap();
            let symbol = core::cmp::max(
                -127,
                core::cmp::min(127, dist.inverse_cdf(quantile).round() as i32),
            );

            symbols_gaussian.push(symbol);
            means.push(mean);
            stds.push(std_dev);
        }

        let hist = [
            1u32, 186545, 237403, 295700, 361445, 433686, 509456, 586943, 663946, 737772, 1657269,
            896675, 922197, 930672, 916665, 0, 0, 0, 0, 0, 723031, 650522, 572300, 494702, 418703,
            347600, 1, 283500, 226158, 178194, 136301, 103158, 76823, 55540, 39258, 27988, 54269,
        ];
        let categorical_probabilities = hist.iter().map(|&x| x as f64).collect::<Vec<_>>();
        let categorical = Categorical::<Probability, PRECISION>::from_floating_point_probabilities(
            &categorical_probabilities,
        )
        .unwrap();
        let mut symbols_categorical = Vec::with_capacity(AMT);
        let max_probability = Probability::max_value() >> (Probability::BITS - PRECISION);
        for _ in 0..AMT {
            let quantile = rng.next_u32().as_() & max_probability;
            let symbol = categorical.quantile_function(quantile).0;
            symbols_categorical.push(symbol);
        }

        let mut encoder = RangeEncoder::<CompressedWord, State>::new();

        encoder
            .encode_iid_symbols(&symbols_categorical, &categorical)
            .unwrap();
        dbg!(
            encoder.num_bits(),
            AMT as f64 * categorical.entropy::<f64>()
        );

        let quantizer = LeakyQuantizer::<_, _, Probability, PRECISION>::new(-127..=127);
        encoder
            .encode_symbols(symbols_gaussian.iter().zip(&means).zip(&stds).map(
                |((&symbol, &mean), &core)| {
                    (symbol, quantizer.quantize(Normal::new(mean, core).unwrap()))
                },
            ))
            .unwrap();
        dbg!(encoder.num_bits());

        let mut decoder = encoder.into_decoder().unwrap();

        let reconstructed_categorical = decoder
            .decode_iid_symbols(AMT, &categorical)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let reconstructed_gaussian = decoder
            .decode_symbols(
                means
                    .iter()
                    .zip(&stds)
                    .map(|(&mean, &core)| quantizer.quantize(Normal::new(mean, core).unwrap())),
            )
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(decoder.maybe_empty());

        assert_eq!(symbols_categorical, reconstructed_categorical);
        assert_eq!(symbols_gaussian, reconstructed_gaussian);
    }

    #[test]
    fn seek() {
        const NUM_CHUNKS: usize = 100;
        const SYMBOLS_PER_CHUNK: usize = 100;

        let quantizer = LeakyQuantizer::<_, _, u32, 24>::new(-100..=100);
        let model = quantizer.quantize(Normal::new(0.0, 10.0).unwrap());

        let mut encoder = DefaultRangeEncoder::new();

        let mut rng = Xoshiro256StarStar::seed_from_u64(123);
        let mut symbols = Vec::with_capacity(NUM_CHUNKS);
        let mut jump_table = Vec::with_capacity(NUM_CHUNKS);

        for _ in 0..NUM_CHUNKS {
            jump_table.push(encoder.pos_and_state());
            let chunk = (0..SYMBOLS_PER_CHUNK)
                .map(|_| model.quantile_function(rng.next_u32() % (1 << 24)).0)
                .collect::<Vec<_>>();
            encoder.encode_iid_symbols(&chunk, &model).unwrap();
            symbols.push(chunk);
        }
        let final_pos_and_state = encoder.pos_and_state();

        let mut decoder = encoder.decoder();

        // Verify that decoding leads to the same positions and states.
        for (chunk, &pos_and_state) in symbols.iter().zip(&jump_table) {
            assert_eq!(decoder.pos_and_state(), pos_and_state);
            let decoded = decoder
                .decode_iid_symbols(SYMBOLS_PER_CHUNK, &model)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(&decoded, chunk);
        }
        assert_eq!(decoder.pos_and_state(), final_pos_and_state);
        assert!(decoder.maybe_empty());

        // Seek to some random offsets in the jump table and decode one chunk
        for i in 0..100 {
            let chunk_index = if i == 3 {
                // Make sure we test jumping to beginning at least once.
                0
            } else {
                rng.next_u32() as usize % NUM_CHUNKS
            };

            let pos_and_state = jump_table[chunk_index];
            decoder.seek(pos_and_state).unwrap();
            let decoded = decoder
                .decode_iid_symbols(SYMBOLS_PER_CHUNK, &model)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(&decoded, &symbols[chunk_index])
        }

        // Test jumping to end (but first make sure we're not already at the end).
        decoder.seek(jump_table[0]).unwrap();
        assert!(!decoder.maybe_empty());
        decoder.seek(final_pos_and_state).unwrap();
        assert!(decoder.maybe_empty());
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum FrontendError {
    InvalidData,
}

impl Display for FrontendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidData => write!(f, "Tried to decode invalid compressed data."),
        }
    }
}

#[cfg(feature = "std")]
impl Error for FrontendError {}
