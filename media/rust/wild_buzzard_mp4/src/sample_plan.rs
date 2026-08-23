//! Provider-neutral planning for classic non-fragmented MP4 samples.

use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::mem::size_of;
use std::num::NonZeroU64;

use std::collections::hash_map::RandomState;

use super::{
    BoxView, MAX_NESTED_BOXES, MAX_TOP_LEVEL_BOXES, Mp4Initialization, parse_box,
    parse_track_header_identity,
};

/// Maximum samples published for one track.
pub const MAX_PLANNED_SAMPLES_PER_TRACK: usize = 1_000_000;
/// Maximum timing or sample-to-chunk runs accepted from one table.
pub const MAX_SAMPLE_TABLE_RUNS: usize = 131_072;
/// Maximum chunk offsets accepted for one track.
pub const MAX_CHUNKS_PER_TRACK: usize = 262_144;
/// Maximum top-level media-data boxes retained by one plan.
pub const MAX_MEDIA_DATA_BOXES: usize = 1_024;
/// Maximum aggregate logical entries inspected across all sample tables.
pub const MAX_TOTAL_SAMPLE_TABLE_ENTRIES: usize = 4_000_000;
/// Maximum charged planning operations for one call.
pub const MAX_SAMPLE_PLAN_WORK_UNITS: usize = 8_000_000;
/// Maximum logical bytes occupied by the published sample metadata vector.
pub const MAX_SAMPLE_PLAN_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct AdmittedSource<'source> {
    bytes: &'source [u8],
}

impl<'source> AdmittedSource<'source> {
    const fn new(bytes: &'source [u8]) -> Self {
        Self { bytes }
    }

    const fn bytes(self) -> &'source [u8] {
        self.bytes
    }
}

impl fmt::Debug for AdmittedSource<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedSource")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct SourceBinding {
    hashers: [RandomState; 2],
    tags: [u64; 2],
    source_address: usize,
    source_bytes: usize,
}

impl SourceBinding {
    pub(super) fn new(source: &[u8]) -> Self {
        let hashers = [RandomState::new(), RandomState::new()];
        let tags = [
            source_tag(&hashers[0], source),
            source_tag(&hashers[1], source),
        ];
        Self {
            hashers,
            tags,
            source_address: source.as_ptr() as usize,
            source_bytes: source.len(),
        }
    }

    fn is_same_admission(&self, other: &Self) -> bool {
        self.source_bytes == other.source_bytes && self.tags == other.tags
    }

    fn authenticates(&self, source: &[u8]) -> bool {
        self.source_address == source.as_ptr() as usize
            && self.source_bytes == source.len()
            && self.tags[0] == source_tag(&self.hashers[0], source)
            && self.tags[1] == source_tag(&self.hashers[1], source)
    }
}

impl fmt::Debug for SourceBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceBinding")
            .field("source_bytes", &self.source_bytes)
            .finish_non_exhaustive()
    }
}

fn source_tag(hasher: &RandomState, source: &[u8]) -> u64 {
    let mut state = hasher.build_hasher();
    state.write_usize(source.len());
    state.write(source);
    state.finish()
}

/// Opaque identity for one exact track in one initialization admission.
///
/// Values are created only by [`super::admit_complete_mp4`] and carry a private per-admission
/// source seal. Planning re-authenticates the complete caller-owned bytes before using the token.
#[derive(Clone)]
pub struct TrackIdentity {
    source_binding: SourceBinding,
    ordinal: usize,
    id: u32,
}

impl TrackIdentity {
    pub(super) const fn new(source_binding: SourceBinding, ordinal: usize, id: u32) -> Self {
        Self {
            source_binding,
            ordinal,
            id,
        }
    }

    /// Return the nonzero `tkhd` track identifier authenticated at admission.
    pub const fn id(&self) -> u32 {
        self.id
    }

    fn is_same_token(&self, other: &Self) -> bool {
        self.id == other.id
            && self.ordinal == other.ordinal
            && self.source_binding.is_same_admission(&other.source_binding)
    }
}

impl PartialEq for TrackIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.is_same_token(other)
    }
}

impl Eq for TrackIdentity {}

impl fmt::Debug for TrackIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrackIdentity")
            .field("track_id", &self.id)
            .field("track_ordinal", &self.ordinal)
            .field("source_bytes", &self.source_binding.source_bytes)
            .finish()
    }
}

/// A half-open byte range authenticated to one `mdat` payload in the admitted source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleByteRange {
    /// Absolute inclusive byte offset in the complete source.
    start: u64,
    /// Absolute exclusive byte offset in the complete source.
    end: u64,
}

impl SampleByteRange {
    /// Return the absolute inclusive byte offset in the complete source.
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Return the absolute exclusive byte offset in the complete source.
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Return the exact number of payload bytes in this authenticated range.
    pub const fn len(self) -> u64 {
        self.end - self.start
    }

    /// Return whether this range contains no bytes.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// One exact track-local timestamp before edit-list transformation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackTimestamp {
    /// Signed track-local units.
    pub units: i64,
    /// Nonzero units per second.
    pub timescale: NonZeroU64,
}

/// One exact sample duration in track-local units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleDuration {
    /// Unsigned duration units from `stts`.
    pub units: u32,
    /// Nonzero units per second.
    pub timescale: NonZeroU64,
}

/// One provider-neutral classic MP4 sample description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedSample {
    /// Authenticated source range containing the compressed sample bytes.
    pub byte_range: SampleByteRange,
    /// Decode timestamp derived from the cumulative `stts` deltas.
    pub decode_timestamp: TrackTimestamp,
    /// Composition timestamp derived from decode time and optional `ctts` offset.
    pub composition_timestamp: TrackTimestamp,
    /// Decode duration derived from the current `stts` run.
    pub duration: SampleDuration,
    /// Whether the sample is listed in `stss`, or true when `stss` is absent.
    pub is_sync: bool,
    /// One-based `stsd` entry selected by the active `stsc` run.
    pub sample_description_index: u32,
}

/// Exact bounded-work accounting for one successful sample plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SamplePlanAccounting {
    /// Complete borrowed source bytes.
    pub source_bytes: usize,
    /// Top-level `mdat` payloads inspected.
    pub media_data_boxes: usize,
    /// Chunk offsets consumed for the selected track.
    pub chunks: usize,
    /// Samples published.
    pub samples: usize,
    /// `stts` runs consumed.
    pub decode_time_runs: usize,
    /// `ctts` runs consumed, or zero when the table is absent.
    pub composition_time_runs: usize,
    /// `stsc` runs consumed.
    pub sample_to_chunk_runs: usize,
    /// `stss` entries consumed, or zero when the table is absent.
    pub sync_sample_entries: usize,
    /// Aggregate logical sample-table entries charged.
    pub table_entries: usize,
    /// Logical bytes occupied by the published sample metadata.
    pub planned_sample_bytes: usize,
    /// Explicit planning work units charged.
    pub work_units: usize,
}

/// Immutable, source-borrowing plan for all samples in one classic MP4 track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackSamplePlan<'source> {
    source: AdmittedSource<'source>,
    track: TrackIdentity,
    timescale: NonZeroU64,
    samples: Vec<PlannedSample>,
    accounting: SamplePlanAccounting,
}

impl<'source> TrackSamplePlan<'source> {
    /// Return the exact source-bound track identity represented by this plan.
    pub fn track_identity(&self) -> TrackIdentity {
        self.track.clone()
    }

    /// Return the selected track's nonzero units per second.
    pub const fn timescale(&self) -> NonZeroU64 {
        self.timescale
    }

    /// Enumerate immutable sample metadata in decode order.
    pub fn samples(&self) -> &[PlannedSample] {
        &self.samples
    }

    /// Borrow one sample's compressed bytes from the original caller-owned source without copying.
    pub fn sample_bytes(&self, sample_index: usize) -> Option<&'source [u8]> {
        let range = self.samples.get(sample_index)?.byte_range;
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        self.source.bytes().get(start..end)
    }

    /// Return exact successful resource accounting.
    pub const fn accounting(&self) -> SamplePlanAccounting {
        self.accounting
    }
}

/// Sample tables recognized by the classic planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleTable {
    /// Decode timing (`stts`).
    DecodeTime,
    /// Optional composition timing (`ctts`).
    CompositionTime,
    /// Sample-to-chunk mapping (`stsc`).
    SampleToChunk,
    /// Full or compact sample sizes (`stsz` or `stz2`).
    SampleSize,
    /// 32-bit or 64-bit chunk offsets (`stco` or `co64`).
    ChunkOffset,
    /// Optional sync-sample numbers (`stss`).
    SyncSample,
}

/// Cross-table cardinality relationships checked before publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleCardinality {
    /// `stts` does not describe exactly the sample-size count.
    DecodeTime,
    /// Present `ctts` does not describe exactly the sample-size count.
    CompositionTime,
    /// Expanded `stsc` chunks do not describe exactly the sample-size count.
    ChunkMapping,
}

/// Fixed resource classes enforced by the sample planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplePlanResource {
    /// Structural box inspection exceeded the admitted structural ceiling.
    StructuralBoxes,
    /// Top-level media-data boxes exceeded [`MAX_MEDIA_DATA_BOXES`].
    MediaDataBoxes,
    /// A timing or sample-to-chunk table exceeded [`MAX_SAMPLE_TABLE_RUNS`].
    TableRuns,
    /// Chunk offsets exceeded [`MAX_CHUNKS_PER_TRACK`].
    Chunks,
    /// Logical samples exceeded [`MAX_PLANNED_SAMPLES_PER_TRACK`].
    Samples,
    /// Aggregate table entries exceeded [`MAX_TOTAL_SAMPLE_TABLE_ENTRIES`].
    AggregateTableEntries,
    /// Published metadata exceeded [`MAX_SAMPLE_PLAN_BYTES`].
    PlanBytes,
    /// Charged work exceeded [`MAX_SAMPLE_PLAN_WORK_UNITS`].
    Work,
}

/// Typed and redacted classic-sample planning failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SamplePlanError {
    /// The complete bytes or private admission seal do not match the selected initialization.
    SourceBindingMismatch,
    /// The token does not identify the exact admitted track slot and `tkhd` identity.
    TrackBindingMismatch,
    /// A movie-fragment box or fragment-extension declaration was present.
    FragmentedSource,
    /// The admitted source's ISO-BMFF structure could not be revalidated.
    MalformedStructure,
    /// A required classic table is absent.
    MissingTable(SampleTable),
    /// A singleton classic table is duplicated or conflicts with its alternate form.
    DuplicateTable(SampleTable),
    /// A classic table has invalid version, flags, framing, ordering, or values.
    MalformedTable(SampleTable),
    /// Cross-table expanded sample counts disagree.
    CardinalityMismatch(SampleCardinality),
    /// An `stsc` run names no admitted one-based sample description.
    InvalidSampleDescriptionIndex,
    /// A fixed resource ceiling was exceeded.
    ResourceLimitExceeded(SamplePlanResource),
    /// Track-local timestamp arithmetic overflowed.
    TimestampOverflow,
    /// Sample byte-range arithmetic overflowed.
    ByteRangeOverflow,
    /// A sample byte range escaped the complete source.
    ByteRangeOutsideSource,
    /// A sample byte range was not wholly inside one top-level `mdat` payload.
    ByteRangeOutsideMediaData,
    /// A bounded metadata allocation failed.
    OutOfMemory,
}

impl fmt::Display for SamplePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SourceBindingMismatch => "sample source binding mismatch",
            Self::TrackBindingMismatch => "sample track binding mismatch",
            Self::FragmentedSource => "fragmented MP4 is not supported",
            Self::MalformedStructure => "malformed ISO-BMFF structure",
            Self::MissingTable(_) => "required classic sample table is missing",
            Self::DuplicateTable(_) => "classic sample table is duplicated",
            Self::MalformedTable(_) => "classic sample table is malformed",
            Self::CardinalityMismatch(_) => "classic sample-table cardinality mismatch",
            Self::InvalidSampleDescriptionIndex => "invalid sample-description index",
            Self::ResourceLimitExceeded(_) => "sample-plan resource policy exceeded",
            Self::TimestampOverflow => "sample timestamp overflow",
            Self::ByteRangeOverflow => "sample byte-range overflow",
            Self::ByteRangeOutsideSource => "sample byte range escapes source",
            Self::ByteRangeOutsideMediaData => "sample byte range escapes media data",
            Self::OutOfMemory => "bounded sample-plan allocation failed",
        })
    }
}

impl std::error::Error for SamplePlanError {}

#[derive(Clone, Copy)]
struct SourceRange {
    start: u64,
    end: u64,
}

struct LocatedTrack {
    sample_table: BoxView,
    media_data: Vec<SourceRange>,
}

#[derive(Default)]
struct PlanningBudget {
    boxes: usize,
    table_entries: usize,
    work_units: usize,
}

impl PlanningBudget {
    fn inspect_box(&mut self) -> Result<(), SamplePlanError> {
        self.boxes = self
            .boxes
            .checked_add(1)
            .ok_or(SamplePlanError::ResourceLimitExceeded(
                SamplePlanResource::StructuralBoxes,
            ))?;
        if self.boxes > MAX_TOP_LEVEL_BOXES + MAX_NESTED_BOXES {
            return Err(SamplePlanError::ResourceLimitExceeded(
                SamplePlanResource::StructuralBoxes,
            ));
        }
        self.charge_work(1)
    }

    fn charge_table(
        &mut self,
        entries: usize,
        per_table_limit: usize,
        resource: SamplePlanResource,
    ) -> Result<(), SamplePlanError> {
        if entries > per_table_limit {
            return Err(SamplePlanError::ResourceLimitExceeded(resource));
        }
        self.table_entries = self.table_entries.checked_add(entries).ok_or(
            SamplePlanError::ResourceLimitExceeded(SamplePlanResource::AggregateTableEntries),
        )?;
        if self.table_entries > MAX_TOTAL_SAMPLE_TABLE_ENTRIES {
            return Err(SamplePlanError::ResourceLimitExceeded(
                SamplePlanResource::AggregateTableEntries,
            ));
        }
        self.charge_work(entries)
    }

    fn charge_work(&mut self, units: usize) -> Result<(), SamplePlanError> {
        self.work_units =
            self.work_units
                .checked_add(units)
                .ok_or(SamplePlanError::ResourceLimitExceeded(
                    SamplePlanResource::Work,
                ))?;
        if self.work_units > MAX_SAMPLE_PLAN_WORK_UNITS {
            return Err(SamplePlanError::ResourceLimitExceeded(
                SamplePlanResource::Work,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum SampleSizeView {
    Full(BoxView),
    Compact(BoxView),
}

struct TableViews {
    decode_time: BoxView,
    composition_time: Option<BoxView>,
    sample_to_chunk: BoxView,
    sample_size: SampleSizeView,
    chunk_offset: BoxView,
    chunk_offset_is_64_bit: bool,
    sync_sample: Option<BoxView>,
}

#[derive(Clone, Copy)]
struct TimeRun {
    count: u32,
    delta: u32,
}

#[derive(Clone, Copy)]
struct CompositionRun {
    count: u32,
    offset: i64,
}

#[derive(Clone, Copy)]
struct ChunkRun {
    first_chunk: u32,
    samples_per_chunk: u32,
    description_index: u32,
}

enum SampleSizes {
    Constant { size: u32, count: usize },
    Variable(Vec<u32>),
}

impl SampleSizes {
    fn len(&self) -> usize {
        match self {
            Self::Constant { count, .. } => *count,
            Self::Variable(values) => values.len(),
        }
    }

    fn get(&self, index: usize) -> Option<u32> {
        match self {
            Self::Constant { size, count } => (index < *count).then_some(*size),
            Self::Variable(values) => values.get(index).copied(),
        }
    }
}

/// Plan every sample of one exact admitted non-fragmented MP4 track.
///
/// The returned plan borrows the caller-owned `source` after authenticating it against
/// `initialization`; it copies no compressed sample payload. Every range is checked against both
/// the complete source and one exact top-level `mdat` payload. Timestamps are track-local and do
/// not apply edit lists.
pub fn plan_non_fragmented_track_samples<'source>(
    source: &'source [u8],
    initialization: &Mp4Initialization,
    track: TrackIdentity,
) -> Result<TrackSamplePlan<'source>, SamplePlanError> {
    if !initialization
        .source_binding
        .is_same_admission(&track.source_binding)
        || !initialization.source_binding.authenticates(source)
    {
        return Err(SamplePlanError::SourceBindingMismatch);
    }
    let metadata = initialization
        .tracks
        .get(track.ordinal)
        .ok_or(SamplePlanError::TrackBindingMismatch)?;
    if metadata.id != track.id || !metadata.identity().is_same_token(&track) {
        return Err(SamplePlanError::TrackBindingMismatch);
    }

    let mut budget = PlanningBudget::default();
    let located = locate_track(source, &track, &mut budget)?;
    let views = locate_tables(source, located.sample_table, &mut budget)?;
    let sizes = parse_sample_sizes(source, views.sample_size, &mut budget)?;
    let sample_count = sizes.len();
    let decode_runs = parse_decode_time(source, views.decode_time, &mut budget)?;
    let composition_runs = views
        .composition_time
        .map(|view| parse_composition_time(source, view, &mut budget))
        .transpose()?;
    let chunk_runs = parse_sample_to_chunk(source, views.sample_to_chunk, &mut budget)?;
    let chunk_offsets = parse_chunk_offsets(
        source,
        views.chunk_offset,
        views.chunk_offset_is_64_bit,
        &mut budget,
    )?;
    let sync_samples = views
        .sync_sample
        .map(|view| parse_sync_samples(source, view, &mut budget))
        .transpose()?;

    validate_timing_cardinality(&decode_runs, sample_count, SampleCardinality::DecodeTime)?;
    if let Some(runs) = composition_runs.as_deref() {
        validate_composition_cardinality(runs, sample_count)?;
    }
    validate_chunk_cardinality(
        &chunk_runs,
        chunk_offsets.len(),
        sample_count,
        metadata.sample_descriptions.len(),
        &mut budget,
    )?;
    if let Some(sync) = sync_samples.as_deref() {
        validate_sync_samples(sync, sample_count, &mut budget)?;
    }
    if sample_count != 0 && located.media_data.is_empty() {
        return Err(SamplePlanError::ByteRangeOutsideMediaData);
    }

    let planned_sample_bytes = sample_count.checked_mul(size_of::<PlannedSample>()).ok_or(
        SamplePlanError::ResourceLimitExceeded(SamplePlanResource::PlanBytes),
    )?;
    if planned_sample_bytes > MAX_SAMPLE_PLAN_BYTES {
        return Err(SamplePlanError::ResourceLimitExceeded(
            SamplePlanResource::PlanBytes,
        ));
    }
    let mut samples = bounded_vec(sample_count)?;
    let mut decode_cursor = TimeCursor::new(&decode_runs);
    let mut composition_cursor = composition_runs.as_deref().map(CompositionCursor::new);
    let mut decode_timestamp = 0i64;
    let mut sync_cursor = 0usize;
    let mut sample_index = 0usize;
    let mut chunk_run_index = 0usize;
    let source_end = u64::try_from(source.len()).map_err(|_| SamplePlanError::ByteRangeOverflow)?;

    for (chunk_index, &chunk_start) in chunk_offsets.iter().enumerate() {
        let chunk_number = u32::try_from(chunk_index + 1)
            .map_err(|_| SamplePlanError::CardinalityMismatch(SampleCardinality::ChunkMapping))?;
        while chunk_run_index + 1 < chunk_runs.len()
            && chunk_runs[chunk_run_index + 1].first_chunk == chunk_number
        {
            chunk_run_index += 1;
        }
        let run = chunk_runs
            .get(chunk_run_index)
            .ok_or(SamplePlanError::CardinalityMismatch(
                SampleCardinality::ChunkMapping,
            ))?;
        let mut position = chunk_start;
        for _ in 0..run.samples_per_chunk {
            budget.charge_work(1)?;
            let size = sizes
                .get(sample_index)
                .ok_or(SamplePlanError::CardinalityMismatch(
                    SampleCardinality::ChunkMapping,
                ))?;
            let end = position
                .checked_add(u64::from(size))
                .ok_or(SamplePlanError::ByteRangeOverflow)?;
            let byte_range = SampleByteRange {
                start: position,
                end,
            };
            if end > source_end {
                return Err(SamplePlanError::ByteRangeOutsideSource);
            }
            if !range_is_in_media_data(&located.media_data, byte_range, &mut budget)? {
                return Err(SamplePlanError::ByteRangeOutsideMediaData);
            }

            let duration = decode_cursor
                .next()
                .ok_or(SamplePlanError::CardinalityMismatch(
                    SampleCardinality::DecodeTime,
                ))?;
            let composition_offset = match composition_cursor.as_mut() {
                Some(cursor) => cursor.next().ok_or(SamplePlanError::CardinalityMismatch(
                    SampleCardinality::CompositionTime,
                ))?,
                None => 0,
            };
            let composition_timestamp = decode_timestamp
                .checked_add(composition_offset)
                .ok_or(SamplePlanError::TimestampOverflow)?;
            let next_decode = decode_timestamp
                .checked_add(i64::from(duration))
                .ok_or(SamplePlanError::TimestampOverflow)?;
            let sample_number = u32::try_from(sample_index + 1)
                .map_err(|_| SamplePlanError::ResourceLimitExceeded(SamplePlanResource::Samples))?;
            let is_sync = match sync_samples.as_deref() {
                Some(sync) if sync.get(sync_cursor) == Some(&sample_number) => {
                    sync_cursor += 1;
                    true
                }
                Some(_) => false,
                None => true,
            };

            samples.push(PlannedSample {
                byte_range,
                decode_timestamp: TrackTimestamp {
                    units: decode_timestamp,
                    timescale: metadata.timescale,
                },
                composition_timestamp: TrackTimestamp {
                    units: composition_timestamp,
                    timescale: metadata.timescale,
                },
                duration: SampleDuration {
                    units: duration,
                    timescale: metadata.timescale,
                },
                is_sync,
                sample_description_index: run.description_index,
            });
            sample_index += 1;
            position = end;
            decode_timestamp = next_decode;
        }
    }

    if sample_index != sample_count
        || decode_cursor.next().is_some()
        || composition_cursor
            .as_mut()
            .is_some_and(|cursor| cursor.next().is_some())
        || sync_samples
            .as_ref()
            .is_some_and(|sync| sync_cursor != sync.len())
    {
        return Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::ChunkMapping,
        ));
    }

    let accounting = SamplePlanAccounting {
        source_bytes: source.len(),
        media_data_boxes: located.media_data.len(),
        chunks: chunk_offsets.len(),
        samples: sample_count,
        decode_time_runs: decode_runs.len(),
        composition_time_runs: composition_runs.as_ref().map_or(0, Vec::len),
        sample_to_chunk_runs: chunk_runs.len(),
        sync_sample_entries: sync_samples.as_ref().map_or(0, Vec::len),
        table_entries: budget.table_entries,
        planned_sample_bytes,
        work_units: budget.work_units,
    };
    Ok(TrackSamplePlan {
        source: AdmittedSource::new(source),
        track,
        timescale: metadata.timescale,
        samples,
        accounting,
    })
}

fn locate_track(
    source: &[u8],
    track: &TrackIdentity,
    budget: &mut PlanningBudget,
) -> Result<LocatedTrack, SamplePlanError> {
    let mut offset = 0usize;
    let mut movie = None;
    let mut media_data = Vec::new();
    media_data
        .try_reserve_exact(4)
        .map_err(|_| SamplePlanError::OutOfMemory)?;
    while offset < source.len() {
        budget.inspect_box()?;
        let view = planning_box(source, offset, source.len())?;
        match &view.kind {
            b"moov" => {
                if movie.replace(view).is_some() {
                    return Err(SamplePlanError::MalformedStructure);
                }
            }
            b"moof" => return Err(SamplePlanError::FragmentedSource),
            b"mdat" => {
                if media_data.len() >= MAX_MEDIA_DATA_BOXES {
                    return Err(SamplePlanError::ResourceLimitExceeded(
                        SamplePlanResource::MediaDataBoxes,
                    ));
                }
                media_data
                    .try_reserve(1)
                    .map_err(|_| SamplePlanError::OutOfMemory)?;
                media_data.push(SourceRange {
                    start: u64::try_from(view.payload_start)
                        .map_err(|_| SamplePlanError::ByteRangeOverflow)?,
                    end: u64::try_from(view.end).map_err(|_| SamplePlanError::ByteRangeOverflow)?,
                });
            }
            _ => {}
        }
        offset = view.end;
    }
    let movie = movie.ok_or(SamplePlanError::MalformedStructure)?;
    let sample_table = locate_movie_track(source, movie, track, budget)?;
    Ok(LocatedTrack {
        sample_table,
        media_data,
    })
}

fn locate_movie_track(
    source: &[u8],
    movie: BoxView,
    track: &TrackIdentity,
    budget: &mut PlanningBudget,
) -> Result<BoxView, SamplePlanError> {
    let mut offset = movie.payload_start;
    let mut ordinal = 0usize;
    let mut selected = None;
    while offset < movie.end {
        budget.inspect_box()?;
        let view = planning_box(source, offset, movie.end)?;
        match &view.kind {
            b"mvex" => return Err(SamplePlanError::FragmentedSource),
            b"trak" => {
                if ordinal == track.ordinal {
                    selected = Some(locate_sample_table(source, view, track.id, budget)?);
                }
                ordinal = ordinal
                    .checked_add(1)
                    .ok_or(SamplePlanError::MalformedStructure)?;
            }
            _ => {}
        }
        offset = view.end;
    }
    selected.ok_or(SamplePlanError::TrackBindingMismatch)
}

fn locate_sample_table(
    source: &[u8],
    track: BoxView,
    expected_id: u32,
    budget: &mut PlanningBudget,
) -> Result<BoxView, SamplePlanError> {
    let mut offset = track.payload_start;
    let mut identity = None;
    let mut media = None;
    while offset < track.end {
        budget.inspect_box()?;
        let view = planning_box(source, offset, track.end)?;
        match &view.kind {
            b"tkhd" => {
                if identity.is_some() {
                    return Err(SamplePlanError::MalformedStructure);
                }
                identity = Some(
                    parse_track_header_identity(source, view)
                        .map_err(|_| SamplePlanError::MalformedStructure)?,
                );
            }
            b"mdia" => {
                if media.replace(view).is_some() {
                    return Err(SamplePlanError::MalformedStructure);
                }
            }
            _ => {}
        }
        offset = view.end;
    }
    if identity != Some(expected_id) {
        return Err(SamplePlanError::TrackBindingMismatch);
    }
    let media = media.ok_or(SamplePlanError::MalformedStructure)?;
    let information = locate_single_child(source, media, *b"minf", budget)?;
    locate_single_child(source, information, *b"stbl", budget)
}

fn locate_single_child(
    source: &[u8],
    parent: BoxView,
    kind: [u8; 4],
    budget: &mut PlanningBudget,
) -> Result<BoxView, SamplePlanError> {
    let mut offset = parent.payload_start;
    let mut found = None;
    while offset < parent.end {
        budget.inspect_box()?;
        let view = planning_box(source, offset, parent.end)?;
        if view.kind == kind && found.replace(view).is_some() {
            return Err(SamplePlanError::MalformedStructure);
        }
        offset = view.end;
    }
    found.ok_or(SamplePlanError::MalformedStructure)
}

fn locate_tables(
    source: &[u8],
    sample_table: BoxView,
    budget: &mut PlanningBudget,
) -> Result<TableViews, SamplePlanError> {
    let mut decode_time = None;
    let mut composition_time = None;
    let mut sample_to_chunk = None;
    let mut sample_size = None;
    let mut chunk_offset = None;
    let mut sync_sample = None;
    let mut offset = sample_table.payload_start;
    while offset < sample_table.end {
        budget.inspect_box()?;
        let view = planning_box(source, offset, sample_table.end)?;
        match &view.kind {
            b"stts" => set_table(&mut decode_time, view, SampleTable::DecodeTime)?,
            b"ctts" => set_table(&mut composition_time, view, SampleTable::CompositionTime)?,
            b"stsc" => set_table(&mut sample_to_chunk, view, SampleTable::SampleToChunk)?,
            b"stsz" => {
                if sample_size.replace(SampleSizeView::Full(view)).is_some() {
                    return Err(SamplePlanError::DuplicateTable(SampleTable::SampleSize));
                }
            }
            b"stz2" => {
                if sample_size.replace(SampleSizeView::Compact(view)).is_some() {
                    return Err(SamplePlanError::DuplicateTable(SampleTable::SampleSize));
                }
            }
            b"stco" => {
                if chunk_offset.replace((view, false)).is_some() {
                    return Err(SamplePlanError::DuplicateTable(SampleTable::ChunkOffset));
                }
            }
            b"co64" => {
                if chunk_offset.replace((view, true)).is_some() {
                    return Err(SamplePlanError::DuplicateTable(SampleTable::ChunkOffset));
                }
            }
            b"stss" => set_table(&mut sync_sample, view, SampleTable::SyncSample)?,
            _ => {}
        }
        offset = view.end;
    }
    let decode_time = decode_time.ok_or(SamplePlanError::MissingTable(SampleTable::DecodeTime))?;
    let sample_to_chunk =
        sample_to_chunk.ok_or(SamplePlanError::MissingTable(SampleTable::SampleToChunk))?;
    let sample_size = sample_size.ok_or(SamplePlanError::MissingTable(SampleTable::SampleSize))?;
    let (chunk_offset, chunk_offset_is_64_bit) =
        chunk_offset.ok_or(SamplePlanError::MissingTable(SampleTable::ChunkOffset))?;
    Ok(TableViews {
        decode_time,
        composition_time,
        sample_to_chunk,
        sample_size,
        chunk_offset,
        chunk_offset_is_64_bit,
        sync_sample,
    })
}

fn set_table(
    slot: &mut Option<BoxView>,
    view: BoxView,
    table: SampleTable,
) -> Result<(), SamplePlanError> {
    if slot.replace(view).is_some() {
        return Err(SamplePlanError::DuplicateTable(table));
    }
    Ok(())
}

fn parse_sample_sizes(
    source: &[u8],
    view: SampleSizeView,
    budget: &mut PlanningBudget,
) -> Result<SampleSizes, SamplePlanError> {
    match view {
        SampleSizeView::Full(view) => parse_full_sample_sizes(source, view, budget),
        SampleSizeView::Compact(view) => parse_compact_sample_sizes(source, view, budget),
    }
}

fn parse_full_sample_sizes(
    source: &[u8],
    view: BoxView,
    budget: &mut PlanningBudget,
) -> Result<SampleSizes, SamplePlanError> {
    let mut offset = full_box_body(source, view, SampleTable::SampleSize, &[0])?;
    let sample_size = table_u32(source, offset, view.end, SampleTable::SampleSize)?;
    offset = checked_advance(offset, 4, SampleTable::SampleSize)?;
    let count = table_count(source, offset, view.end, SampleTable::SampleSize)?;
    offset = checked_advance(offset, 4, SampleTable::SampleSize)?;
    budget.charge_table(
        count,
        MAX_PLANNED_SAMPLES_PER_TRACK,
        SamplePlanResource::Samples,
    )?;
    if sample_size != 0 {
        require_exact_end(offset, count, 0, view.end, SampleTable::SampleSize)?;
        return Ok(SampleSizes::Constant {
            size: sample_size,
            count,
        });
    }
    require_exact_end(offset, count, 4, view.end, SampleTable::SampleSize)?;
    let mut values = bounded_vec(count)?;
    for _ in 0..count {
        values.push(table_u32(
            source,
            offset,
            view.end,
            SampleTable::SampleSize,
        )?);
        offset = checked_advance(offset, 4, SampleTable::SampleSize)?;
    }
    Ok(SampleSizes::Variable(values))
}

fn parse_compact_sample_sizes(
    source: &[u8],
    view: BoxView,
    budget: &mut PlanningBudget,
) -> Result<SampleSizes, SamplePlanError> {
    let mut offset = full_box_body(source, view, SampleTable::SampleSize, &[0])?;
    let reserved_end = checked_advance(offset, 3, SampleTable::SampleSize)?;
    if source.get(offset..reserved_end) != Some(&[0, 0, 0]) {
        return Err(SamplePlanError::MalformedTable(SampleTable::SampleSize));
    }
    offset = reserved_end;
    let field_size = *source
        .get(offset)
        .ok_or(SamplePlanError::MalformedTable(SampleTable::SampleSize))?;
    offset = checked_advance(offset, 1, SampleTable::SampleSize)?;
    if !matches!(field_size, 4 | 8 | 16) {
        return Err(SamplePlanError::MalformedTable(SampleTable::SampleSize));
    }
    let count = table_count(source, offset, view.end, SampleTable::SampleSize)?;
    offset = checked_advance(offset, 4, SampleTable::SampleSize)?;
    budget.charge_table(
        count,
        MAX_PLANNED_SAMPLES_PER_TRACK,
        SamplePlanResource::Samples,
    )?;
    let bit_count = count
        .checked_mul(usize::from(field_size))
        .ok_or(SamplePlanError::MalformedTable(SampleTable::SampleSize))?;
    let byte_count = bit_count
        .checked_add(7)
        .ok_or(SamplePlanError::MalformedTable(SampleTable::SampleSize))?
        / 8;
    require_exact_end(offset, byte_count, 1, view.end, SampleTable::SampleSize)?;
    let bytes = source
        .get(offset..view.end)
        .ok_or(SamplePlanError::MalformedTable(SampleTable::SampleSize))?;
    let mut values = bounded_vec(count)?;
    match field_size {
        4 => {
            for index in 0..count {
                let byte = bytes[index / 2];
                values.push(if index % 2 == 0 {
                    u32::from(byte >> 4)
                } else {
                    u32::from(byte & 0x0f)
                });
            }
        }
        8 => values.extend(bytes.iter().take(count).map(|value| u32::from(*value))),
        16 => {
            for index in 0..count {
                let start = index * 2;
                values.push(u32::from(u16::from_be_bytes([
                    bytes[start],
                    bytes[start + 1],
                ])));
            }
        }
        _ => unreachable!(),
    }
    Ok(SampleSizes::Variable(values))
}

fn parse_decode_time(
    source: &[u8],
    view: BoxView,
    budget: &mut PlanningBudget,
) -> Result<Vec<TimeRun>, SamplePlanError> {
    let mut offset = full_box_body(source, view, SampleTable::DecodeTime, &[0])?;
    let count = table_count(source, offset, view.end, SampleTable::DecodeTime)?;
    offset = checked_advance(offset, 4, SampleTable::DecodeTime)?;
    budget.charge_table(count, MAX_SAMPLE_TABLE_RUNS, SamplePlanResource::TableRuns)?;
    require_exact_end(offset, count, 8, view.end, SampleTable::DecodeTime)?;
    let mut runs = bounded_vec(count)?;
    for _ in 0..count {
        let sample_count = table_u32(source, offset, view.end, SampleTable::DecodeTime)?;
        let delta = table_u32(
            source,
            checked_advance(offset, 4, SampleTable::DecodeTime)?,
            view.end,
            SampleTable::DecodeTime,
        )?;
        if sample_count == 0 {
            return Err(SamplePlanError::MalformedTable(SampleTable::DecodeTime));
        }
        runs.push(TimeRun {
            count: sample_count,
            delta,
        });
        offset = checked_advance(offset, 8, SampleTable::DecodeTime)?;
    }
    Ok(runs)
}

fn parse_composition_time(
    source: &[u8],
    view: BoxView,
    budget: &mut PlanningBudget,
) -> Result<Vec<CompositionRun>, SamplePlanError> {
    let version = full_box_version(source, view, SampleTable::CompositionTime, &[0, 1])?;
    let mut offset = checked_advance(view.payload_start, 4, SampleTable::CompositionTime)?;
    let count = table_count(source, offset, view.end, SampleTable::CompositionTime)?;
    offset = checked_advance(offset, 4, SampleTable::CompositionTime)?;
    budget.charge_table(count, MAX_SAMPLE_TABLE_RUNS, SamplePlanResource::TableRuns)?;
    require_exact_end(offset, count, 8, view.end, SampleTable::CompositionTime)?;
    let mut runs = bounded_vec(count)?;
    for _ in 0..count {
        let sample_count = table_u32(source, offset, view.end, SampleTable::CompositionTime)?;
        let raw_offset = table_u32(
            source,
            checked_advance(offset, 4, SampleTable::CompositionTime)?,
            view.end,
            SampleTable::CompositionTime,
        )?;
        if sample_count == 0 {
            return Err(SamplePlanError::MalformedTable(
                SampleTable::CompositionTime,
            ));
        }
        let time_offset = if version == 0 {
            i64::from(raw_offset)
        } else {
            i64::from(raw_offset as i32)
        };
        runs.push(CompositionRun {
            count: sample_count,
            offset: time_offset,
        });
        offset = checked_advance(offset, 8, SampleTable::CompositionTime)?;
    }
    Ok(runs)
}

fn parse_sample_to_chunk(
    source: &[u8],
    view: BoxView,
    budget: &mut PlanningBudget,
) -> Result<Vec<ChunkRun>, SamplePlanError> {
    let mut offset = full_box_body(source, view, SampleTable::SampleToChunk, &[0])?;
    let count = table_count(source, offset, view.end, SampleTable::SampleToChunk)?;
    offset = checked_advance(offset, 4, SampleTable::SampleToChunk)?;
    budget.charge_table(count, MAX_SAMPLE_TABLE_RUNS, SamplePlanResource::TableRuns)?;
    require_exact_end(offset, count, 12, view.end, SampleTable::SampleToChunk)?;
    let mut runs = bounded_vec(count)?;
    for _ in 0..count {
        let first_chunk = table_u32(source, offset, view.end, SampleTable::SampleToChunk)?;
        let samples_per_chunk = table_u32(
            source,
            checked_advance(offset, 4, SampleTable::SampleToChunk)?,
            view.end,
            SampleTable::SampleToChunk,
        )?;
        let description_index = table_u32(
            source,
            checked_advance(offset, 8, SampleTable::SampleToChunk)?,
            view.end,
            SampleTable::SampleToChunk,
        )?;
        if first_chunk == 0 || samples_per_chunk == 0 || description_index == 0 {
            return Err(SamplePlanError::MalformedTable(SampleTable::SampleToChunk));
        }
        runs.push(ChunkRun {
            first_chunk,
            samples_per_chunk,
            description_index,
        });
        offset = checked_advance(offset, 12, SampleTable::SampleToChunk)?;
    }
    Ok(runs)
}

fn parse_chunk_offsets(
    source: &[u8],
    view: BoxView,
    is_64_bit: bool,
    budget: &mut PlanningBudget,
) -> Result<Vec<u64>, SamplePlanError> {
    let mut offset = full_box_body(source, view, SampleTable::ChunkOffset, &[0])?;
    let count = table_count(source, offset, view.end, SampleTable::ChunkOffset)?;
    offset = checked_advance(offset, 4, SampleTable::ChunkOffset)?;
    budget.charge_table(count, MAX_CHUNKS_PER_TRACK, SamplePlanResource::Chunks)?;
    let width = if is_64_bit { 8 } else { 4 };
    require_exact_end(offset, count, width, view.end, SampleTable::ChunkOffset)?;
    let mut offsets = bounded_vec(count)?;
    for _ in 0..count {
        offsets.push(if is_64_bit {
            table_u64(source, offset, view.end, SampleTable::ChunkOffset)?
        } else {
            u64::from(table_u32(
                source,
                offset,
                view.end,
                SampleTable::ChunkOffset,
            )?)
        });
        offset = checked_advance(offset, width, SampleTable::ChunkOffset)?;
    }
    Ok(offsets)
}

fn parse_sync_samples(
    source: &[u8],
    view: BoxView,
    budget: &mut PlanningBudget,
) -> Result<Vec<u32>, SamplePlanError> {
    let mut offset = full_box_body(source, view, SampleTable::SyncSample, &[0])?;
    let count = table_count(source, offset, view.end, SampleTable::SyncSample)?;
    offset = checked_advance(offset, 4, SampleTable::SyncSample)?;
    budget.charge_table(
        count,
        MAX_PLANNED_SAMPLES_PER_TRACK,
        SamplePlanResource::Samples,
    )?;
    require_exact_end(offset, count, 4, view.end, SampleTable::SyncSample)?;
    let mut samples = bounded_vec(count)?;
    for _ in 0..count {
        samples.push(table_u32(
            source,
            offset,
            view.end,
            SampleTable::SyncSample,
        )?);
        offset = checked_advance(offset, 4, SampleTable::SyncSample)?;
    }
    Ok(samples)
}

fn validate_timing_cardinality(
    runs: &[TimeRun],
    sample_count: usize,
    mismatch: SampleCardinality,
) -> Result<(), SamplePlanError> {
    let total = runs.iter().try_fold(0usize, |total, run| {
        usize::try_from(run.count)
            .ok()
            .and_then(|count| total.checked_add(count))
    });
    if total != Some(sample_count) {
        return Err(SamplePlanError::CardinalityMismatch(mismatch));
    }
    Ok(())
}

fn validate_composition_cardinality(
    runs: &[CompositionRun],
    sample_count: usize,
) -> Result<(), SamplePlanError> {
    let total = runs.iter().try_fold(0usize, |total, run| {
        usize::try_from(run.count)
            .ok()
            .and_then(|count| total.checked_add(count))
    });
    if total != Some(sample_count) {
        return Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::CompositionTime,
        ));
    }
    Ok(())
}

fn validate_chunk_cardinality(
    runs: &[ChunkRun],
    chunk_count: usize,
    sample_count: usize,
    description_count: usize,
    budget: &mut PlanningBudget,
) -> Result<(), SamplePlanError> {
    if sample_count == 0 {
        if runs.is_empty() && chunk_count == 0 {
            return Ok(());
        }
        return Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::ChunkMapping,
        ));
    }
    if runs.first().map(|run| run.first_chunk) != Some(1) || chunk_count == 0 {
        return Err(SamplePlanError::MalformedTable(SampleTable::SampleToChunk));
    }
    let mut total = 0u64;
    for (index, run) in runs.iter().enumerate() {
        budget.charge_work(1)?;
        if usize::try_from(run.description_index)
            .ok()
            .is_none_or(|description| description == 0 || description > description_count)
        {
            return Err(SamplePlanError::InvalidSampleDescriptionIndex);
        }
        let first = usize::try_from(run.first_chunk)
            .map_err(|_| SamplePlanError::MalformedTable(SampleTable::SampleToChunk))?;
        let next = match runs.get(index + 1) {
            Some(next) => {
                let next = usize::try_from(next.first_chunk)
                    .map_err(|_| SamplePlanError::MalformedTable(SampleTable::SampleToChunk))?;
                if next <= first {
                    return Err(SamplePlanError::MalformedTable(SampleTable::SampleToChunk));
                }
                next
            }
            None => chunk_count + 1,
        };
        if first > chunk_count || next > chunk_count + 1 {
            return Err(SamplePlanError::MalformedTable(SampleTable::SampleToChunk));
        }
        let chunks = u64::try_from(next - first)
            .map_err(|_| SamplePlanError::CardinalityMismatch(SampleCardinality::ChunkMapping))?;
        total = total
            .checked_add(chunks.checked_mul(u64::from(run.samples_per_chunk)).ok_or(
                SamplePlanError::CardinalityMismatch(SampleCardinality::ChunkMapping),
            )?)
            .ok_or(SamplePlanError::CardinalityMismatch(
                SampleCardinality::ChunkMapping,
            ))?;
    }
    if total != sample_count as u64 {
        return Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::ChunkMapping,
        ));
    }
    Ok(())
}

fn validate_sync_samples(
    samples: &[u32],
    sample_count: usize,
    budget: &mut PlanningBudget,
) -> Result<(), SamplePlanError> {
    let maximum = u32::try_from(sample_count)
        .map_err(|_| SamplePlanError::ResourceLimitExceeded(SamplePlanResource::Samples))?;
    let mut previous = 0u32;
    for &sample in samples {
        budget.charge_work(1)?;
        if sample == 0 || sample > maximum || sample <= previous {
            return Err(SamplePlanError::MalformedTable(SampleTable::SyncSample));
        }
        previous = sample;
    }
    Ok(())
}

struct TimeCursor<'a> {
    runs: &'a [TimeRun],
    index: usize,
    remaining: u32,
}

impl<'a> TimeCursor<'a> {
    const fn new(runs: &'a [TimeRun]) -> Self {
        Self {
            runs,
            index: 0,
            remaining: 0,
        }
    }

    fn next(&mut self) -> Option<u32> {
        if self.remaining == 0 {
            let run = self.runs.get(self.index)?;
            self.index += 1;
            self.remaining = run.count;
        }
        self.remaining -= 1;
        Some(self.runs[self.index - 1].delta)
    }
}

struct CompositionCursor<'a> {
    runs: &'a [CompositionRun],
    index: usize,
    remaining: u32,
}

impl<'a> CompositionCursor<'a> {
    const fn new(runs: &'a [CompositionRun]) -> Self {
        Self {
            runs,
            index: 0,
            remaining: 0,
        }
    }

    fn next(&mut self) -> Option<i64> {
        if self.remaining == 0 {
            let run = self.runs.get(self.index)?;
            self.index += 1;
            self.remaining = run.count;
        }
        self.remaining -= 1;
        Some(self.runs[self.index - 1].offset)
    }
}

fn range_is_in_media_data(
    ranges: &[SourceRange],
    sample: SampleByteRange,
    budget: &mut PlanningBudget,
) -> Result<bool, SamplePlanError> {
    let mut low = 0usize;
    let mut high = ranges.len();
    while low < high {
        budget.charge_work(1)?;
        let middle = low + (high - low) / 2;
        if ranges[middle].start <= sample.start {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low != 0 && {
        let range = ranges[low - 1];
        sample.start >= range.start && sample.end <= range.end
    })
}

fn planning_box(source: &[u8], start: usize, end: usize) -> Result<BoxView, SamplePlanError> {
    parse_box(source, start, end).map_err(|_| SamplePlanError::MalformedStructure)
}

fn full_box_body(
    source: &[u8],
    view: BoxView,
    table: SampleTable,
    versions: &[u8],
) -> Result<usize, SamplePlanError> {
    full_box_version(source, view, table, versions)?;
    checked_advance(view.payload_start, 4, table)
}

fn full_box_version(
    source: &[u8],
    view: BoxView,
    table: SampleTable,
    versions: &[u8],
) -> Result<u8, SamplePlanError> {
    let end = checked_advance(view.payload_start, 4, table)?;
    let fields = source
        .get(view.payload_start..end)
        .ok_or(SamplePlanError::MalformedTable(table))?;
    if !versions.contains(&fields[0]) || fields[1..] != [0, 0, 0] {
        return Err(SamplePlanError::MalformedTable(table));
    }
    Ok(fields[0])
}

fn table_count(
    source: &[u8],
    offset: usize,
    end: usize,
    table: SampleTable,
) -> Result<usize, SamplePlanError> {
    usize::try_from(table_u32(source, offset, end, table)?)
        .map_err(|_| SamplePlanError::MalformedTable(table))
}

fn table_u32(
    source: &[u8],
    offset: usize,
    end: usize,
    table: SampleTable,
) -> Result<u32, SamplePlanError> {
    let value_end = offset
        .checked_add(4)
        .ok_or(SamplePlanError::MalformedTable(table))?;
    let bytes: [u8; 4] = source
        .get(offset..value_end.min(end))
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(SamplePlanError::MalformedTable(table))?;
    Ok(u32::from_be_bytes(bytes))
}

fn table_u64(
    source: &[u8],
    offset: usize,
    end: usize,
    table: SampleTable,
) -> Result<u64, SamplePlanError> {
    let value_end = offset
        .checked_add(8)
        .ok_or(SamplePlanError::MalformedTable(table))?;
    let bytes: [u8; 8] = source
        .get(offset..value_end.min(end))
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(SamplePlanError::MalformedTable(table))?;
    Ok(u64::from_be_bytes(bytes))
}

fn checked_advance(
    offset: usize,
    bytes: usize,
    table: SampleTable,
) -> Result<usize, SamplePlanError> {
    offset
        .checked_add(bytes)
        .ok_or(SamplePlanError::MalformedTable(table))
}

fn require_exact_end(
    entries_start: usize,
    count: usize,
    width: usize,
    expected_end: usize,
    table: SampleTable,
) -> Result<(), SamplePlanError> {
    let end = count
        .checked_mul(width)
        .and_then(|bytes| entries_start.checked_add(bytes));
    if end != Some(expected_end) {
        return Err(SamplePlanError::MalformedTable(table));
    }
    Ok(())
}

fn bounded_vec<T>(capacity: usize) -> Result<Vec<T>, SamplePlanError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|_| SamplePlanError::OutOfMemory)?;
    Ok(values)
}
