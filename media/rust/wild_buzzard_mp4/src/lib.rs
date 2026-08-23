//! Bounded admission and classic-sample planning for complete ISO-BMFF sources.
//!
//! This crate publishes provider-neutral initialization metadata and an authenticated,
//! non-copying plan for samples in classic non-fragmented tracks. It does not decode media,
//! access files or networks, decrypt protected content, or admit untrusted browser content into
//! the product.

#![deny(missing_docs, unsafe_code)]

use std::fmt;
use std::io::Cursor;
use std::num::NonZeroU64;

mod sample_plan;

use sample_plan::SourceBinding;
pub use sample_plan::{
    MAX_CHUNKS_PER_TRACK, MAX_MEDIA_DATA_BOXES, MAX_PLANNED_SAMPLES_PER_TRACK,
    MAX_SAMPLE_PLAN_BYTES, MAX_SAMPLE_PLAN_WORK_UNITS, MAX_SAMPLE_TABLE_RUNS,
    MAX_TOTAL_SAMPLE_TABLE_ENTRIES, PlannedSample, SampleByteRange, SampleCardinality,
    SampleDuration, SamplePlanAccounting, SamplePlanError, SamplePlanResource, SampleTable,
    TrackIdentity, TrackSamplePlan, TrackTimestamp, plan_non_fragmented_track_samples,
};

/// Maximum complete source accepted by [`admit_complete_mp4`].
pub const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum number of top-level boxes inspected before parser admission.
pub const MAX_TOP_LEVEL_BOXES: usize = 4_096;
/// Maximum number of direct children across all movie boxes.
pub const MAX_MOVIE_CHILD_BOXES: usize = 4_096;
/// Maximum number of box headers inspected below the top level.
pub const MAX_NESTED_BOXES: usize = 8_192;
/// Maximum number of tracks accepted in one source.
pub const MAX_TRACKS: usize = 32;
/// Maximum number of compatible brands, excluding the major brand.
pub const MAX_COMPATIBLE_BRANDS: usize = 63;
/// Maximum sample descriptions accepted per track.
pub const MAX_SAMPLE_DESCRIPTIONS_PER_TRACK: usize = 16;
/// Maximum bytes copied from one codec configuration descriptor.
pub const MAX_CODEC_CONFIG_BYTES: usize = 1024 * 1024;
/// Maximum codec configuration bytes published across one admission.
pub const MAX_PUBLISHED_CONFIG_BYTES: usize = 4 * 1024 * 1024;
/// Maximum aggregate codec-box payload bytes inspected before parser admission.
pub const MAX_DECLARED_CONFIG_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of AVC parameter sets accepted in one decoder configuration.
pub const MAX_AVC_PARAMETER_SETS: usize = 64;
/// Maximum bytes accepted in one AVC parameter set.
pub const MAX_AVC_PARAMETER_SET_BYTES: usize = 64 * 1024;
/// Maximum number of protection-system headers accepted in one movie.
pub const MAX_PSSH_BOXES: usize = 16;
/// Maximum bytes accepted in one protection-system header box payload.
pub const MAX_PSSH_BYTES: usize = 1024 * 1024;
/// Maximum aggregate protection-system header payload bytes per admission.
pub const MAX_TOTAL_PSSH_BYTES: usize = 4 * 1024 * 1024;
/// Maximum key identifiers framed by one version-1 protection-system header.
pub const MAX_PSSH_KEY_IDS: usize = 64;
/// Maximum audio channel count accepted by this initial boundary.
pub const MAX_AUDIO_CHANNELS: u32 = 64;
/// Maximum audio sample rate accepted by this initial boundary.
pub const MAX_AUDIO_SAMPLE_RATE_HZ: u32 = 768_000;

/// Product admission remains disabled until mp4parse has a browser-owned deep allocation budget.
pub const UNTRUSTED_CONTENT_ADMISSION_ENABLED: bool = false;

/// A four-byte ISO-BMFF identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FourCc(pub [u8; 4]);

/// Authenticated brands from the source's file-type box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerBrands {
    /// Major brand.
    pub major: FourCc,
    /// Compatible brands in source order.
    pub compatible: Vec<FourCc>,
}

/// Provider-neutral admitted initialization metadata.
#[derive(Clone, Debug)]
pub struct Mp4Initialization {
    /// Container brands authenticated from the complete source.
    pub brands: ContainerBrands,
    /// Movie timescale in units per second.
    pub movie_timescale: NonZeroU64,
    /// Admitted tracks in source order.
    pub tracks: Vec<TrackMetadata>,
    /// Whether any admitted sample description is protected or encrypted.
    pub protection_present: bool,
    /// Exact accounting for bounded input and published collections.
    pub accounting: AdmissionAccounting,
    source_binding: SourceBinding,
}

impl PartialEq for Mp4Initialization {
    fn eq(&self, other: &Self) -> bool {
        self.brands == other.brands
            && self.movie_timescale == other.movie_timescale
            && self.tracks == other.tracks
            && self.protection_present == other.protection_present
            && self.accounting == other.accounting
    }
}

impl Eq for Mp4Initialization {}

/// Resource accounting produced only after complete successful admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionAccounting {
    /// Complete source bytes passed to mp4parse.
    pub source_bytes: usize,
    /// Top-level box headers inspected by the wrapper.
    pub top_level_boxes: usize,
    /// Direct movie-child box headers inspected by the wrapper.
    pub movie_child_boxes: usize,
    /// Box headers inspected below the top level, including direct movie children.
    pub nested_boxes: usize,
    /// Compatible brands published, excluding the major brand.
    pub compatible_brands: usize,
    /// Tracks published.
    pub tracks: usize,
    /// Sample descriptions published across all tracks.
    pub sample_descriptions: usize,
    /// Codec configuration bytes copied into the result.
    pub published_config_bytes: usize,
    /// Codec-box payload bytes structurally inspected before parser admission.
    pub declared_config_bytes: usize,
    /// Global movie-level protection-system header boxes inspected before parser admission.
    pub protection_system_headers: usize,
    /// Global movie-level protection-system header payload bytes inspected before parser admission.
    pub protection_system_header_bytes: usize,
}

/// An admitted media track.
#[derive(Clone, Debug)]
pub struct TrackMetadata {
    /// Nonzero identity from `tkhd`.
    pub id: u32,
    /// Provider-neutral track kind.
    pub kind: TrackKind,
    /// Nonzero track-local timescale in units per second.
    pub timescale: NonZeroU64,
    /// Track-local duration, when the source declares one.
    pub duration: Option<ScaledDuration>,
    /// Display dimensions in ISO-BMFF 16.16 fixed-point units, when declared.
    pub display_dimensions: Option<FixedDimensions>,
    /// Admitted sample descriptions in source order.
    pub sample_descriptions: Vec<SampleDescription>,
    identity: TrackIdentity,
}

impl TrackMetadata {
    /// Return the opaque source-bound identity required to plan this exact track's samples.
    pub fn identity(&self) -> TrackIdentity {
        self.identity.clone()
    }
}

impl PartialEq for TrackMetadata {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.timescale == other.timescale
            && self.duration == other.duration
            && self.display_dimensions == other.display_dimensions
            && self.sample_descriptions == other.sample_descriptions
    }
}

impl Eq for TrackMetadata {}

/// Browser-relevant track kinds surfaced by this initial boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    /// Audio media.
    Audio,
    /// Video media.
    Video,
    /// Still-picture media.
    Picture,
    /// Auxiliary video media.
    AuxiliaryVideo,
}

/// A checked duration represented both exactly and in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaledDuration {
    /// Exact duration units from the source.
    pub units: u64,
    /// Nonzero units per second.
    pub timescale: NonZeroU64,
    /// Checked floor conversion to nanoseconds.
    pub nanoseconds: u64,
}

/// Dimensions encoded as unsigned 16.16 fixed-point values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedDimensions {
    /// Width in unsigned 16.16 fixed-point units.
    pub width_16_16: u32,
    /// Height in unsigned 16.16 fixed-point units.
    pub height_16_16: u32,
}

/// One decoder-facing sample description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleDescription {
    /// Codec family authenticated by the parser and protection metadata.
    pub codec: CodecFamily,
    /// Bounded codec initialization bytes, when the parser exposes them.
    pub decoder_config: Option<Vec<u8>>,
    /// Audio format fields for audio descriptions.
    pub audio: Option<AudioFormat>,
    /// Video format fields for video descriptions.
    pub video: Option<VideoFormat>,
    /// Whether this description contains protection metadata.
    pub protected: bool,
}

/// Generic codec families represented by the pinned parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecFamily {
    /// MPEG-1/2 Layer III audio.
    Mp3,
    /// Advanced Audio Coding.
    Aac,
    /// Extended High-Efficiency AAC.
    XheAac,
    /// Free Lossless Audio Codec.
    Flac,
    /// Opus audio.
    Opus,
    /// Linear PCM audio.
    LinearPcm,
    /// Apple Lossless Audio Codec.
    Alac,
    /// AVC/H.264 video.
    H264,
    /// MPEG-4 Part 2 video.
    Mpeg4Visual,
    /// AV1 video.
    Av1,
    /// VP9 video.
    Vp9,
    /// VP8 video.
    Vp8,
    /// H.263 video.
    H263,
    /// HEVC/H.265 video.
    Hevc,
}

/// Audio initialization fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    /// Number of channels.
    pub channels: u32,
    /// Integral sample rate in hertz.
    pub sample_rate_hz: u32,
    /// Declared bits per sample.
    pub bits_per_sample: u16,
}

/// Video initialization fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    /// Coded width in pixels.
    pub coded_width: u16,
    /// Coded height in pixels.
    pub coded_height: u16,
}

/// A redacted parser failure class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParserFailure {
    /// The source ended before a declared structure completed.
    Truncated,
    /// The source contains malformed parser-level data.
    Malformed,
    /// The parser does not support a required construct.
    Unsupported,
    /// No movie box was found.
    MissingMovie,
    /// A reader operation failed.
    Input,
    /// The parser could not allocate required memory.
    OutOfMemory,
}

/// Required singleton boxes in the admitted initialization hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequiredBox {
    /// Movie header (`mvhd`).
    MovieHeader,
    /// Track header (`tkhd`).
    TrackHeader,
    /// Track media container (`mdia`).
    TrackMedia,
    /// Media header (`mdhd`).
    MediaHeader,
    /// Media handler (`hdlr`).
    Handler,
    /// Media-information container (`minf`).
    MediaInformation,
    /// Sample-table container (`stbl`).
    SampleTable,
    /// Sample-description table (`stsd`).
    SampleDescription,
}

/// Typed protection-metadata failures with no key or initialization bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectionFailure {
    /// A protected entry omitted its protection-information box.
    Missing,
    /// More than one protection record or singleton protection child was present.
    Duplicate,
    /// Required protection fields were absent or malformed.
    Incomplete,
    /// Protection records disagreed with each other.
    Conflicting,
    /// The original format is incompatible with the authenticated codec configuration.
    CodecMismatch,
}

/// Typed failures while framing the MPEG-4 elementary-stream descriptor hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EsDescriptorFailure {
    /// A descriptor or one of its bounded lengths is malformed or not exactly framed.
    Malformed,
    /// The `esds` payload does not contain an elementary-stream descriptor.
    MissingElementaryStream,
    /// A second payload-identical elementary-stream descriptor was declared.
    DuplicateElementaryStream,
    /// A second differing elementary-stream descriptor was declared.
    ConflictingElementaryStream,
    /// The elementary-stream descriptor does not contain a decoder configuration.
    MissingDecoderConfig,
    /// A second payload-identical decoder configuration was declared.
    DuplicateDecoderConfig,
    /// A second differing decoder configuration was declared.
    ConflictingDecoderConfig,
    /// An AAC decoder configuration does not contain decoder-specific information.
    MissingDecoderSpecificInfo,
    /// A second byte-identical decoder-specific-information record was declared.
    DuplicateDecoderSpecificInfo,
    /// A second differing decoder-specific-information record was declared.
    ConflictingDecoderSpecificInfo,
    /// The exact decoder-specific bytes returned by the parser differ from preflight.
    ParserDecoderSpecificMismatch,
}

/// Typed, redacted admission failures. No source bytes or upstream diagnostics are retained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    /// The complete source is empty.
    EmptySource,
    /// The source exceeds [`MAX_SOURCE_BYTES`].
    SourceTooLarge,
    /// Top-level box framing is malformed, truncated, or ambiguous.
    MalformedStructure,
    /// No file-type box was found before the movie data.
    MissingFileType,
    /// More than one file-type box was found.
    DuplicateFileType,
    /// More than one movie box was found; this boundary does not merge movies.
    DuplicateMovie,
    /// The file-type box appeared after a movie box.
    FileTypeAfterMovie,
    /// The top-level box count exceeds [`MAX_TOP_LEVEL_BOXES`].
    TooManyTopLevelBoxes,
    /// Direct movie children exceed [`MAX_MOVIE_CHILD_BOXES`].
    TooManyMovieChildren,
    /// Nested box inspection exceeds [`MAX_NESTED_BOXES`].
    TooManyNestedBoxes,
    /// Tracks exceed [`MAX_TRACKS`].
    TooManyTracks,
    /// The source contains no track eligible for metadata admission.
    NoTracks,
    /// The wrapper and parser observed different direct track counts.
    TrackCountMismatch,
    /// A required singleton box is absent from its exact parent.
    MissingRequiredBox(RequiredBox),
    /// A required singleton box occurs more than once in its exact parent.
    DuplicateRequiredBox(RequiredBox),
    /// A recognized initialization box occurs under the wrong parent.
    InvalidTrackHierarchy,
    /// Required initialization boxes occur in an order the pinned parser cannot interpret safely.
    InvalidBoxOrder,
    /// Compatible brands exceed [`MAX_COMPATIBLE_BRANDS`].
    TooManyCompatibleBrands,
    /// The pinned parser rejected the source.
    Parser(ParserFailure),
    /// The parser did not publish a nonzero movie timescale.
    InvalidMovieTimescale,
    /// A track has no usable identity.
    InvalidTrackIdentity,
    /// Two tracks publish the same identity.
    DuplicateTrackIdentity,
    /// A track kind is outside this initial audio/video admission boundary.
    UnsupportedTrackKind,
    /// A track has no valid nonzero local timescale.
    InvalidTrackTimescale,
    /// A checked duration conversion overflowed.
    TimeOverflow,
    /// A track has no sample descriptions.
    MissingSampleDescription,
    /// Sample descriptions exceed [`MAX_SAMPLE_DESCRIPTIONS_PER_TRACK`].
    TooManySampleDescriptions,
    /// Declared, framed, and parser-published sample-description counts disagree.
    SampleDescriptionCountMismatch,
    /// A sample description does not match its track kind.
    SampleDescriptionKindMismatch,
    /// The parser could not identify a usable codec family.
    UnsupportedCodec,
    /// A recognized codec family is deliberately disabled at this admission gate.
    UnsupportedCodecConfiguration,
    /// A required decoder configuration is missing or duplicated.
    InvalidCodecConfigurationQuantity,
    /// A decoder configuration is structurally malformed.
    MalformedCodecConfiguration,
    /// An MPEG-4 elementary-stream descriptor hierarchy is invalid or ambiguous.
    EsDescriptor(EsDescriptorFailure),
    /// Protection metadata is absent, duplicated, incomplete, or incompatible.
    Protection(ProtectionFailure),
    /// A coded or display dimension is impossible for its track kind.
    InvalidDimensions,
    /// Audio channel count is zero or exceeds [`MAX_AUDIO_CHANNELS`].
    InvalidChannelCount,
    /// Audio sample rate is nonintegral, nonfinite, zero, or too large.
    InvalidSampleRate,
    /// One codec configuration exceeds [`MAX_CODEC_CONFIG_BYTES`].
    CodecConfigurationTooLarge,
    /// Published codec configuration bytes exceed [`MAX_PUBLISHED_CONFIG_BYTES`].
    PublishedConfigurationBudgetExceeded,
    /// Declared codec-box payloads exceed [`MAX_DECLARED_CONFIG_BYTES`].
    DeclaredConfigurationBudgetExceeded,
    /// Protection-system header count or byte policy was exceeded.
    ProtectionSystemHeaderPolicyExceeded,
    /// A bounded result allocation failed.
    OutOfMemory,
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptySource => "empty media source",
            Self::SourceTooLarge => "media source exceeds policy",
            Self::MalformedStructure => "malformed ISO-BMFF structure",
            Self::MissingFileType => "missing file type",
            Self::DuplicateFileType => "duplicate file type",
            Self::DuplicateMovie => "duplicate movie box",
            Self::FileTypeAfterMovie => "file type follows movie data",
            Self::TooManyTopLevelBoxes => "top-level box policy exceeded",
            Self::TooManyMovieChildren => "movie-child box policy exceeded",
            Self::TooManyNestedBoxes => "nested box policy exceeded",
            Self::TooManyTracks => "track policy exceeded",
            Self::NoTracks => "media source contains no tracks",
            Self::TrackCountMismatch => "track structure mismatch",
            Self::MissingRequiredBox(_) => "required initialization box is missing",
            Self::DuplicateRequiredBox(_) => "singleton initialization box is duplicated",
            Self::InvalidTrackHierarchy => "invalid track box hierarchy",
            Self::InvalidBoxOrder => "invalid initialization box order",
            Self::TooManyCompatibleBrands => "brand policy exceeded",
            Self::Parser(_) => "media parser rejected source",
            Self::InvalidMovieTimescale => "invalid movie timescale",
            Self::InvalidTrackIdentity => "invalid track identity",
            Self::DuplicateTrackIdentity => "duplicate track identity",
            Self::UnsupportedTrackKind => "unsupported track kind",
            Self::InvalidTrackTimescale => "invalid track timescale",
            Self::TimeOverflow => "media time conversion overflow",
            Self::MissingSampleDescription => "missing sample description",
            Self::TooManySampleDescriptions => "sample-description policy exceeded",
            Self::SampleDescriptionCountMismatch => "sample-description count mismatch",
            Self::SampleDescriptionKindMismatch => "sample-description kind mismatch",
            Self::UnsupportedCodec => "unsupported media codec",
            Self::UnsupportedCodecConfiguration => "codec configuration is not admitted",
            Self::InvalidCodecConfigurationQuantity => "invalid codec-configuration quantity",
            Self::MalformedCodecConfiguration => "malformed codec configuration",
            Self::EsDescriptor(_) => "invalid elementary-stream descriptor",
            Self::Protection(_) => "invalid protection metadata",
            Self::InvalidDimensions => "invalid media dimensions",
            Self::InvalidChannelCount => "invalid audio channel count",
            Self::InvalidSampleRate => "invalid audio sample rate",
            Self::CodecConfigurationTooLarge => "codec configuration exceeds policy",
            Self::PublishedConfigurationBudgetExceeded => "published configuration budget exceeded",
            Self::DeclaredConfigurationBudgetExceeded => "declared configuration budget exceeded",
            Self::ProtectionSystemHeaderPolicyExceeded => {
                "protection-system-header policy exceeded"
            }
            Self::OutOfMemory => "bounded admission allocation failed",
        })
    }
}

impl std::error::Error for AdmissionError {}

struct Preflight {
    brands: ContainerBrands,
    top_level_boxes: usize,
    movie_child_boxes: usize,
    nested_boxes: usize,
    tracks: Vec<PreflightTrack>,
    declared_config_bytes: usize,
    protection_system_headers: usize,
    protection_system_header_bytes: usize,
}

struct PreflightTrack {
    id: u32,
    kind: TrackKind,
    descriptions: Vec<PreflightDescription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigurationKind {
    BareMp3,
    AudioEsDescriptor,
    Avc,
    Hevc,
    Mpeg4Visual,
    Av1,
    Vpx,
    H263,
}

struct PreflightDescription {
    configuration: ConfigurationKind,
    decoder_specific: Option<ByteRange>,
    protection: Option<PreflightProtection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreflightConfiguration {
    kind: ConfigurationKind,
    decoder_specific: Option<ByteRange>,
}

#[derive(Clone, Copy)]
struct DescriptorView {
    tag: u8,
    payload_start: usize,
    end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PreflightProtection {
    original_format: [u8; 4],
    scheme_type: [u8; 4],
    scheme_version: u32,
    is_encrypted: u8,
    iv_size: u8,
    kid: [u8; 16],
    constant_iv_size: Option<u8>,
    constant_iv: [u8; 16],
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PreflightEncryption {
    is_encrypted: u8,
    iv_size: u8,
    kid: [u8; 16],
    constant_iv_size: Option<u8>,
    constant_iv: [u8; 16],
}

#[derive(Default)]
struct ScanBudget {
    movie_child_boxes: usize,
    nested_boxes: usize,
    declared_config_bytes: usize,
    protection_system_headers: usize,
    protection_system_header_bytes: usize,
}

impl ScanBudget {
    fn inspect_nested_box(&mut self) -> Result<(), AdmissionError> {
        self.nested_boxes = self
            .nested_boxes
            .checked_add(1)
            .ok_or(AdmissionError::TooManyNestedBoxes)?;
        if self.nested_boxes > MAX_NESTED_BOXES {
            return Err(AdmissionError::TooManyNestedBoxes);
        }
        Ok(())
    }

    fn inspect_movie_child(&mut self) -> Result<(), AdmissionError> {
        self.movie_child_boxes = self
            .movie_child_boxes
            .checked_add(1)
            .ok_or(AdmissionError::TooManyMovieChildren)?;
        if self.movie_child_boxes > MAX_MOVIE_CHILD_BOXES {
            return Err(AdmissionError::TooManyMovieChildren);
        }
        self.inspect_nested_box()
    }

    fn charge_configuration(&mut self, bytes: usize) -> Result<(), AdmissionError> {
        if bytes > MAX_CODEC_CONFIG_BYTES {
            return Err(AdmissionError::CodecConfigurationTooLarge);
        }
        self.declared_config_bytes = self
            .declared_config_bytes
            .checked_add(bytes)
            .ok_or(AdmissionError::DeclaredConfigurationBudgetExceeded)?;
        if self.declared_config_bytes > MAX_DECLARED_CONFIG_BYTES {
            return Err(AdmissionError::DeclaredConfigurationBudgetExceeded);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct BoxView {
    kind: [u8; 4],
    payload_start: usize,
    end: usize,
}

/// Admit one caller-owned complete ISO-BMFF source under fixed resource limits.
///
/// The result is published atomically: every check completes before a value is returned. The
/// caller retains ownership of `source`, and this function performs no file or network access.
pub fn admit_complete_mp4(source: &[u8]) -> Result<Mp4Initialization, AdmissionError> {
    let preflight = preflight(source)?;
    let source_binding = SourceBinding::new(source);
    let mut cursor = Cursor::new(source);
    let parsed = mp4parse::read_mp4(&mut cursor, mp4parse::ParseStrictness::Normal)
        .map_err(map_parser_error)?;
    if cursor.position() != source.len() as u64 {
        return Err(AdmissionError::MalformedStructure);
    }

    let movie_timescale = parsed
        .timescale
        .and_then(|value| NonZeroU64::new(value.0))
        .ok_or(AdmissionError::InvalidMovieTimescale)?;
    if parsed.tracks.len() > MAX_TRACKS {
        return Err(AdmissionError::TooManyTracks);
    }
    if parsed.tracks.len() != preflight.tracks.len() {
        return Err(AdmissionError::TrackCountMismatch);
    }
    if parsed.tracks.is_empty() {
        return Err(AdmissionError::NoTracks);
    }

    let mut tracks = Vec::new();
    tracks
        .try_reserve_exact(parsed.tracks.len())
        .map_err(|_| AdmissionError::OutOfMemory)?;
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(parsed.tracks.len())
        .map_err(|_| AdmissionError::OutOfMemory)?;
    let mut sample_descriptions = 0usize;
    let mut published_config_bytes = 0usize;
    if parsed.psshs.len() != preflight.protection_system_headers {
        return Err(AdmissionError::InvalidTrackHierarchy);
    }
    let mut protection_present = false;

    for (track_ordinal, (track, expected_track)) in
        parsed.tracks.iter().zip(&preflight.tracks).enumerate()
    {
        let id = track
            .track_id
            .filter(|identity| *identity != 0)
            .ok_or(AdmissionError::InvalidTrackIdentity)?;
        if id != expected_track.id {
            return Err(AdmissionError::InvalidTrackIdentity);
        }
        if identities.contains(&id) {
            return Err(AdmissionError::DuplicateTrackIdentity);
        }
        identities.push(id);
        let kind = map_track_kind(&track.track_type)?;
        if kind != expected_track.kind {
            return Err(AdmissionError::InvalidTrackHierarchy);
        }
        let raw_timescale = track
            .timescale
            .ok_or(AdmissionError::InvalidTrackTimescale)?;
        if raw_timescale.1 != track.id {
            return Err(AdmissionError::InvalidTrackIdentity);
        }
        let timescale =
            NonZeroU64::new(raw_timescale.0).ok_or(AdmissionError::InvalidTrackTimescale)?;
        let duration = match track.duration {
            Some(duration) => {
                if duration.1 != track.id {
                    return Err(AdmissionError::InvalidTrackIdentity);
                }
                let nanoseconds = duration
                    .0
                    .checked_mul(1_000_000_000)
                    .ok_or(AdmissionError::TimeOverflow)?
                    / timescale.get();
                Some(ScaledDuration {
                    units: duration.0,
                    timescale,
                    nanoseconds,
                })
            }
            None => None,
        };
        let display_dimensions = track.tkhd.as_ref().and_then(|header| {
            (header.width != 0 || header.height != 0).then_some(FixedDimensions {
                width_16_16: header.width,
                height_16_16: header.height,
            })
        });
        if matches!(
            kind,
            TrackKind::Video | TrackKind::Picture | TrackKind::AuxiliaryVideo
        ) {
            let dimensions = display_dimensions.ok_or(AdmissionError::InvalidDimensions)?;
            if dimensions.width_16_16 == 0 || dimensions.height_16_16 == 0 {
                return Err(AdmissionError::InvalidDimensions);
            }
        }

        let source_descriptions = track
            .stsd
            .as_ref()
            .ok_or(AdmissionError::MissingSampleDescription)?;
        if source_descriptions.descriptions.is_empty() {
            return Err(AdmissionError::MissingSampleDescription);
        }
        if source_descriptions.descriptions.len() > MAX_SAMPLE_DESCRIPTIONS_PER_TRACK {
            return Err(AdmissionError::TooManySampleDescriptions);
        }
        if source_descriptions.descriptions.len() != expected_track.descriptions.len() {
            return Err(AdmissionError::SampleDescriptionCountMismatch);
        }
        sample_descriptions = sample_descriptions
            .checked_add(source_descriptions.descriptions.len())
            .ok_or(AdmissionError::PublishedConfigurationBudgetExceeded)?;
        let mut descriptions = Vec::new();
        descriptions
            .try_reserve_exact(source_descriptions.descriptions.len())
            .map_err(|_| AdmissionError::OutOfMemory)?;
        for (description, expected_description) in source_descriptions
            .descriptions
            .iter()
            .zip(&expected_track.descriptions)
        {
            let admitted = admit_description(
                source,
                kind,
                description,
                expected_description,
                &mut published_config_bytes,
            )?;
            protection_present |= admitted.protected;
            descriptions.push(admitted);
        }
        tracks.push(TrackMetadata {
            id,
            kind,
            timescale,
            duration,
            display_dimensions,
            sample_descriptions: descriptions,
            identity: TrackIdentity::new(source_binding.clone(), track_ordinal, id),
        });
    }

    let compatible_brands = preflight.brands.compatible.len();
    Ok(Mp4Initialization {
        brands: preflight.brands,
        movie_timescale,
        tracks,
        protection_present,
        accounting: AdmissionAccounting {
            source_bytes: source.len(),
            top_level_boxes: preflight.top_level_boxes,
            movie_child_boxes: preflight.movie_child_boxes,
            nested_boxes: preflight.nested_boxes,
            compatible_brands,
            tracks: preflight.tracks.len(),
            sample_descriptions,
            published_config_bytes,
            declared_config_bytes: preflight.declared_config_bytes,
            protection_system_headers: preflight.protection_system_headers,
            protection_system_header_bytes: preflight.protection_system_header_bytes,
        },
        source_binding,
    })
}

fn map_parser_error(error: mp4parse::Error) -> AdmissionError {
    let class = match error {
        mp4parse::Error::UnexpectedEOF => ParserFailure::Truncated,
        mp4parse::Error::InvalidData(_) => ParserFailure::Malformed,
        mp4parse::Error::Unsupported(_) => ParserFailure::Unsupported,
        mp4parse::Error::MoovMissing => ParserFailure::MissingMovie,
        mp4parse::Error::Io(_) => ParserFailure::Input,
        mp4parse::Error::OutOfMemory => ParserFailure::OutOfMemory,
    };
    AdmissionError::Parser(class)
}

fn map_track_kind(kind: &mp4parse::TrackType) -> Result<TrackKind, AdmissionError> {
    match kind {
        mp4parse::TrackType::Audio => Ok(TrackKind::Audio),
        mp4parse::TrackType::Video => Ok(TrackKind::Video),
        mp4parse::TrackType::Picture => Ok(TrackKind::Picture),
        mp4parse::TrackType::AuxiliaryVideo => Ok(TrackKind::AuxiliaryVideo),
        mp4parse::TrackType::Metadata | mp4parse::TrackType::Unknown => {
            Err(AdmissionError::UnsupportedTrackKind)
        }
    }
}

fn admit_description(
    source: &[u8],
    track_kind: TrackKind,
    description: &mp4parse::SampleEntry,
    expected: &PreflightDescription,
    total_config_bytes: &mut usize,
) -> Result<SampleDescription, AdmissionError> {
    match description {
        mp4parse::SampleEntry::Audio(audio) if track_kind == TrackKind::Audio => {
            admit_audio_description(source, audio, expected, total_config_bytes)
        }
        mp4parse::SampleEntry::Video(video)
            if matches!(
                track_kind,
                TrackKind::Video | TrackKind::Picture | TrackKind::AuxiliaryVideo
            ) =>
        {
            admit_video_description(video, expected, total_config_bytes)
        }
        mp4parse::SampleEntry::Unknown => Err(AdmissionError::UnsupportedCodec),
        _ => Err(AdmissionError::SampleDescriptionKindMismatch),
    }
}

fn admit_audio_description(
    source: &[u8],
    audio: &mp4parse::AudioSampleEntry,
    expected: &PreflightDescription,
    total_config_bytes: &mut usize,
) -> Result<SampleDescription, AdmissionError> {
    if audio.channelcount == 0 || audio.channelcount > MAX_AUDIO_CHANNELS {
        return Err(AdmissionError::InvalidChannelCount);
    }
    if !audio.samplerate.is_finite()
        || audio.samplerate.fract() != 0.0
        || audio.samplerate < 1.0
        || audio.samplerate > f64::from(MAX_AUDIO_SAMPLE_RATE_HZ)
    {
        return Err(AdmissionError::InvalidSampleRate);
    }
    let (family, config) = match &audio.codec_specific {
        mp4parse::AudioCodecSpecific::ES_Descriptor(descriptor) => {
            if expected.configuration != ConfigurationKind::AudioEsDescriptor {
                return Err(AdmissionError::SampleDescriptionCountMismatch);
            }
            let family = match descriptor.audio_codec {
                mp4parse::CodecType::AAC => CodecFamily::Aac,
                mp4parse::CodecType::XHEAAC => CodecFamily::XheAac,
                mp4parse::CodecType::MP3 => CodecFamily::Mp3,
                _ => return Err(AdmissionError::UnsupportedCodec),
            };
            let config = match family {
                CodecFamily::Aac | CodecFamily::XheAac => {
                    let expected_decoder_specific = expected
                        .decoder_specific
                        .and_then(|range| source.get(range.start..range.end))
                        .ok_or(AdmissionError::EsDescriptor(
                            EsDescriptorFailure::MissingDecoderSpecificInfo,
                        ))?;
                    if descriptor.decoder_specific_data.as_slice() != expected_decoder_specific {
                        return Err(AdmissionError::EsDescriptor(
                            EsDescriptorFailure::ParserDecoderSpecificMismatch,
                        ));
                    }
                    validate_audio_decoder_config(descriptor, audio, family)?;
                    copy_config(&descriptor.decoder_specific_data, total_config_bytes)?
                }
                CodecFamily::Mp3 => {
                    if expected.decoder_specific.is_some()
                        || !descriptor.decoder_specific_data.is_empty()
                    {
                        return Err(AdmissionError::MalformedCodecConfiguration);
                    }
                    None
                }
                _ => return Err(AdmissionError::UnsupportedCodecConfiguration),
            };
            (family, config)
        }
        mp4parse::AudioCodecSpecific::MP3 => {
            if expected.configuration != ConfigurationKind::BareMp3 {
                return Err(AdmissionError::SampleDescriptionCountMismatch);
            }
            (CodecFamily::Mp3, None)
        }
        mp4parse::AudioCodecSpecific::FLACSpecificBox(_)
        | mp4parse::AudioCodecSpecific::OpusSpecificBox(_)
        | mp4parse::AudioCodecSpecific::ALACSpecificBox(_)
        | mp4parse::AudioCodecSpecific::LPCM => {
            return Err(AdmissionError::UnsupportedCodecConfiguration);
        }
    };
    validate_parser_codec(
        audio.codec_type,
        family,
        expected.protection.is_some(),
        true,
    )?;
    validate_parser_protection(expected.protection.as_ref(), &audio.protection_info, family)?;
    Ok(SampleDescription {
        codec: family,
        decoder_config: config,
        audio: Some(AudioFormat {
            channels: audio.channelcount,
            sample_rate_hz: audio.samplerate as u32,
            bits_per_sample: audio.samplesize,
        }),
        video: None,
        protected: expected.protection.is_some(),
    })
}

fn admit_video_description(
    video: &mp4parse::VideoSampleEntry,
    expected: &PreflightDescription,
    total_config_bytes: &mut usize,
) -> Result<SampleDescription, AdmissionError> {
    if video.width == 0 || video.height == 0 {
        return Err(AdmissionError::InvalidDimensions);
    }
    let (family, config) = match &video.codec_specific {
        mp4parse::VideoCodecSpecific::AVCConfig(bytes) => {
            if expected.configuration != ConfigurationKind::Avc {
                return Err(AdmissionError::SampleDescriptionCountMismatch);
            }
            validate_avc_configuration(bytes)?;
            (CodecFamily::H264, copy_config(bytes, total_config_bytes)?)
        }
        mp4parse::VideoCodecSpecific::ESDSConfig(_)
        | mp4parse::VideoCodecSpecific::H263Config(_)
        | mp4parse::VideoCodecSpecific::HEVCConfig(_)
        | mp4parse::VideoCodecSpecific::VPxConfig(_)
        | mp4parse::VideoCodecSpecific::AV1Config(_) => {
            return Err(AdmissionError::UnsupportedCodecConfiguration);
        }
    };
    validate_parser_codec(
        video.codec_type,
        family,
        expected.protection.is_some(),
        false,
    )?;
    validate_parser_protection(expected.protection.as_ref(), &video.protection_info, family)?;
    Ok(SampleDescription {
        codec: family,
        decoder_config: config,
        audio: None,
        video: Some(VideoFormat {
            coded_width: video.width,
            coded_height: video.height,
        }),
        protected: expected.protection.is_some(),
    })
}

fn copy_config(
    source: &[u8],
    total_config_bytes: &mut usize,
) -> Result<Option<Vec<u8>>, AdmissionError> {
    if source.is_empty() {
        return Ok(None);
    }
    if source.len() > MAX_CODEC_CONFIG_BYTES {
        return Err(AdmissionError::CodecConfigurationTooLarge);
    }
    let next_total = total_config_bytes
        .checked_add(source.len())
        .ok_or(AdmissionError::PublishedConfigurationBudgetExceeded)?;
    if next_total > MAX_PUBLISHED_CONFIG_BYTES {
        return Err(AdmissionError::PublishedConfigurationBudgetExceeded);
    }
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(source.len())
        .map_err(|_| AdmissionError::OutOfMemory)?;
    copied.extend_from_slice(source);
    *total_config_bytes = next_total;
    Ok(Some(copied))
}

fn validate_parser_codec(
    parsed: mp4parse::CodecType,
    family: CodecFamily,
    protected: bool,
    audio: bool,
) -> Result<(), AdmissionError> {
    let underlying_matches = matches!(
        (parsed, family),
        (mp4parse::CodecType::MP3, CodecFamily::Mp3)
            | (mp4parse::CodecType::AAC, CodecFamily::Aac)
            | (mp4parse::CodecType::XHEAAC, CodecFamily::XheAac)
            | (mp4parse::CodecType::H264, CodecFamily::H264)
    );
    let encrypted_matches = protected
        && matches!(
            (parsed, audio),
            (mp4parse::CodecType::EncryptedAudio, true)
                | (mp4parse::CodecType::EncryptedVideo, false)
        );
    if underlying_matches || encrypted_matches {
        Ok(())
    } else {
        Err(AdmissionError::UnsupportedCodec)
    }
}

fn validate_parser_protection(
    expected: Option<&PreflightProtection>,
    actual: &[mp4parse::ProtectionSchemeInfoBox],
    family: CodecFamily,
) -> Result<(), AdmissionError> {
    let Some(expected) = expected else {
        return if actual.is_empty() {
            Ok(())
        } else {
            Err(AdmissionError::Protection(ProtectionFailure::Conflicting))
        };
    };
    let [actual] = actual else {
        return Err(AdmissionError::Protection(if actual.is_empty() {
            ProtectionFailure::Missing
        } else {
            ProtectionFailure::Duplicate
        }));
    };
    if actual.original_format.value != expected.original_format {
        return Err(AdmissionError::Protection(ProtectionFailure::Conflicting));
    }
    if !original_format_matches_codec(expected.original_format, family) {
        return Err(AdmissionError::Protection(ProtectionFailure::CodecMismatch));
    }
    let scheme = actual
        .scheme_type
        .as_ref()
        .ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))?;
    if scheme.scheme_type.value != expected.scheme_type
        || scheme.scheme_version != expected.scheme_version
    {
        return Err(AdmissionError::Protection(ProtectionFailure::Conflicting));
    }
    let encryption = actual
        .tenc
        .as_ref()
        .ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))?;
    if encryption.kid.len() != 16
        || encryption.kid.as_slice() != expected.kid
        || encryption.is_encrypted != expected.is_encrypted
        || encryption.iv_size != expected.iv_size
        || encryption
            .constant_iv
            .as_ref()
            .and_then(|iv| u8::try_from(iv.len()).ok())
            != expected.constant_iv_size
        || encryption
            .constant_iv
            .as_ref()
            .is_some_and(|iv| iv.as_slice() != &expected.constant_iv[..iv.len()])
    {
        return Err(AdmissionError::Protection(ProtectionFailure::Conflicting));
    }
    Ok(())
}

fn original_format_matches_codec(format: [u8; 4], family: CodecFamily) -> bool {
    match family {
        CodecFamily::Mp3 => matches!(&format, b"mp4a" | b".mp3"),
        CodecFamily::Aac | CodecFamily::XheAac => &format == b"mp4a",
        CodecFamily::H264 => matches!(&format, b"avc1" | b"avc3"),
        CodecFamily::Hevc => matches!(&format, b"hev1" | b"hvc1"),
        CodecFamily::Av1 => &format == b"av01",
        CodecFamily::Vp8 => &format == b"vp08",
        CodecFamily::Vp9 => &format == b"vp09",
        CodecFamily::Mpeg4Visual => &format == b"mp4v",
        CodecFamily::H263 => &format == b"s263",
        CodecFamily::Flac => &format == b"fLaC",
        CodecFamily::Opus => &format == b"Opus",
        CodecFamily::LinearPcm => &format == b"lpcm",
        CodecFamily::Alac => &format == b"alac",
    }
}

fn validate_audio_decoder_config(
    descriptor: &mp4parse::ES_Descriptor,
    audio: &mp4parse::AudioSampleEntry,
    family: CodecFamily,
) -> Result<(), AdmissionError> {
    let config = descriptor.decoder_specific_data.as_slice();
    if config.is_empty() || config.len() > MAX_CODEC_CONFIG_BYTES {
        return Err(if config.is_empty() {
            AdmissionError::MalformedCodecConfiguration
        } else {
            AdmissionError::CodecConfigurationTooLarge
        });
    }
    let summary = parse_audio_specific_config(config)?;
    let family_matches = match family {
        CodecFamily::Aac => summary.object_type != 42,
        CodecFamily::XheAac => summary.object_type == 42,
        _ => false,
    };
    if !family_matches
        || descriptor.audio_object_type != Some(summary.object_type)
        || descriptor.audio_sample_rate != Some(summary.sample_rate_hz)
        || descriptor.audio_channel_count.is_none()
        || u32::from(descriptor.audio_channel_count.unwrap_or_default()) != audio.channelcount
        || summary
            .channel_count
            .is_some_and(|channels| u32::from(channels) != audio.channelcount)
        || f64::from(summary.sample_rate_hz) != audio.samplerate
    {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    Ok(())
}

struct AudioConfigSummary {
    object_type: u16,
    sample_rate_hz: u32,
    channel_count: Option<u16>,
}

fn parse_audio_specific_config(config: &[u8]) -> Result<AudioConfigSummary, AdmissionError> {
    let mut bits = BitCursor::new(config);
    let mut object_type = bits.read(5)? as u16;
    if object_type == 31 {
        object_type = 32u16
            .checked_add(bits.read(6)? as u16)
            .ok_or(AdmissionError::MalformedCodecConfiguration)?;
    }
    if !matches!(object_type, 1..=4 | 6 | 7 | 17 | 19..=23 | 42) {
        return Err(AdmissionError::UnsupportedCodecConfiguration);
    }
    let frequency_index =
        usize::try_from(bits.read(4)?).map_err(|_| AdmissionError::MalformedCodecConfiguration)?;
    const FREQUENCIES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let sample_rate_hz = if frequency_index == 15 {
        bits.read(24)?
    } else {
        *FREQUENCIES
            .get(frequency_index)
            .ok_or(AdmissionError::MalformedCodecConfiguration)?
    };
    if sample_rate_hz == 0 || sample_rate_hz > MAX_AUDIO_SAMPLE_RATE_HZ {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    let channel_configuration = bits.read(4)? as u16;
    let channel_count = match channel_configuration {
        0 => None,
        1..=7 => Some(channel_configuration),
        11 => Some(7),
        12 | 14 => Some(8),
        _ => return Err(AdmissionError::MalformedCodecConfiguration),
    };
    bits.read(1)?;
    if bits.read(1)? != 0 {
        bits.read(14)?;
    }
    bits.read(1)?;
    Ok(AudioConfigSummary {
        object_type,
        sample_rate_hz,
        channel_count,
    })
}

struct BitCursor<'a> {
    source: &'a [u8],
    position: usize,
}

impl<'a> BitCursor<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            position: 0,
        }
    }

    fn read(&mut self, count: usize) -> Result<u32, AdmissionError> {
        if count > 32
            || self
                .position
                .checked_add(count)
                .is_none_or(|end| end > self.source.len().saturating_mul(8))
        {
            return Err(AdmissionError::MalformedCodecConfiguration);
        }
        let mut value = 0u32;
        for _ in 0..count {
            let byte = self.source[self.position / 8];
            let shift = 7 - (self.position % 8);
            value = (value << 1) | u32::from((byte >> shift) & 1);
            self.position += 1;
        }
        Ok(value)
    }
}

fn validate_avc_configuration(config: &[u8]) -> Result<(), AdmissionError> {
    if config.len() > MAX_CODEC_CONFIG_BYTES {
        return Err(AdmissionError::CodecConfigurationTooLarge);
    }
    if config.len() < 7
        || config[0] != 1
        || config[4] & 0xfc != 0xfc
        || config[4] & 0x03 == 2
        || config[5] & 0xe0 != 0xe0
    {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    let profile = config[1];
    let mut position = 6usize;
    let mut parameter_sets = 0usize;
    let sequence_count = usize::from(config[5] & 0x1f);
    if sequence_count == 0 {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    read_avc_parameter_sets(
        config,
        &mut position,
        sequence_count,
        7,
        &mut parameter_sets,
    )?;
    let picture_count = usize::from(read_config_u8(config, &mut position)?);
    if picture_count == 0 {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    read_avc_parameter_sets(config, &mut position, picture_count, 8, &mut parameter_sets)?;
    if position < config.len() {
        if !matches!(
            profile,
            44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 144
        ) || config.len() - position < 4
            || config[position] & 0xfc != 0xfc
            || config[position + 1] & 0xf8 != 0xf8
            || config[position + 2] & 0xf8 != 0xf8
        {
            return Err(AdmissionError::MalformedCodecConfiguration);
        }
        position += 3;
        let extension_count = usize::from(read_config_u8(config, &mut position)?);
        read_avc_parameter_sets(
            config,
            &mut position,
            extension_count,
            13,
            &mut parameter_sets,
        )?;
    }
    if position != config.len() {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    Ok(())
}

fn read_avc_parameter_sets(
    config: &[u8],
    position: &mut usize,
    count: usize,
    expected_nal_type: u8,
    total_count: &mut usize,
) -> Result<(), AdmissionError> {
    *total_count = total_count
        .checked_add(count)
        .ok_or(AdmissionError::MalformedCodecConfiguration)?;
    if *total_count > MAX_AVC_PARAMETER_SETS {
        return Err(AdmissionError::MalformedCodecConfiguration);
    }
    for _ in 0..count {
        let high = u16::from(read_config_u8(config, position)?);
        let low = u16::from(read_config_u8(config, position)?);
        let length = usize::from((high << 8) | low);
        let end = position
            .checked_add(length)
            .ok_or(AdmissionError::MalformedCodecConfiguration)?;
        if length == 0 || length > MAX_AVC_PARAMETER_SET_BYTES || end > config.len() {
            return Err(AdmissionError::MalformedCodecConfiguration);
        }
        if config[*position] & 0x1f != expected_nal_type {
            return Err(AdmissionError::MalformedCodecConfiguration);
        }
        *position = end;
    }
    Ok(())
}

fn read_config_u8(config: &[u8], position: &mut usize) -> Result<u8, AdmissionError> {
    let value = *config
        .get(*position)
        .ok_or(AdmissionError::MalformedCodecConfiguration)?;
    *position += 1;
    Ok(value)
}

fn preflight(source: &[u8]) -> Result<Preflight, AdmissionError> {
    if source.is_empty() {
        return Err(AdmissionError::EmptySource);
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err(AdmissionError::SourceTooLarge);
    }

    let mut offset = 0usize;
    let mut top_level_boxes = 0usize;
    let mut budget = ScanBudget::default();
    let mut tracks = Vec::new();
    tracks
        .try_reserve_exact(MAX_TRACKS)
        .map_err(|_| AdmissionError::OutOfMemory)?;
    let mut brands = None;
    let mut saw_movie = false;
    while offset < source.len() {
        top_level_boxes = top_level_boxes
            .checked_add(1)
            .ok_or(AdmissionError::TooManyTopLevelBoxes)?;
        if top_level_boxes > MAX_TOP_LEVEL_BOXES {
            return Err(AdmissionError::TooManyTopLevelBoxes);
        }
        let view = parse_box(source, offset, source.len())?;
        match &view.kind {
            b"ftyp" => {
                if brands.is_some() {
                    return Err(AdmissionError::DuplicateFileType);
                }
                if saw_movie {
                    return Err(AdmissionError::FileTypeAfterMovie);
                }
                brands = Some(parse_brands(&source[view.payload_start..view.end])?);
            }
            b"moov" => {
                if saw_movie {
                    return Err(AdmissionError::DuplicateMovie);
                }
                saw_movie = true;
                scan_movie(
                    source,
                    view.payload_start,
                    view.end,
                    &mut budget,
                    &mut tracks,
                )?;
            }
            _ => {}
        }
        offset = view.end;
    }
    let brands = brands.ok_or(AdmissionError::MissingFileType)?;
    if !saw_movie {
        return Err(AdmissionError::Parser(ParserFailure::MissingMovie));
    }
    if tracks.is_empty() {
        return Err(AdmissionError::NoTracks);
    }
    Ok(Preflight {
        brands,
        top_level_boxes,
        movie_child_boxes: budget.movie_child_boxes,
        nested_boxes: budget.nested_boxes,
        tracks,
        declared_config_bytes: budget.declared_config_bytes,
        protection_system_headers: budget.protection_system_headers,
        protection_system_header_bytes: budget.protection_system_header_bytes,
    })
}

fn scan_movie(
    source: &[u8],
    mut offset: usize,
    end: usize,
    budget: &mut ScanBudget,
    tracks: &mut Vec<PreflightTrack>,
) -> Result<(), AdmissionError> {
    let mut movie_header_seen = false;
    while offset < end {
        budget.inspect_movie_child()?;
        let view = parse_box(source, offset, end)?;
        match &view.kind {
            b"mvhd" => {
                if movie_header_seen {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::MovieHeader,
                    ));
                }
                if !tracks.is_empty() {
                    return Err(AdmissionError::InvalidBoxOrder);
                }
                validate_movie_header(source, view)?;
                movie_header_seen = true;
            }
            b"trak" => {
                if !movie_header_seen {
                    return Err(AdmissionError::InvalidBoxOrder);
                }
                if tracks.len() >= MAX_TRACKS {
                    return Err(AdmissionError::TooManyTracks);
                }
                let track = scan_track(source, view.payload_start, view.end, budget)?;
                if tracks.iter().any(|existing| existing.id == track.id) {
                    return Err(AdmissionError::DuplicateTrackIdentity);
                }
                tracks.push(track);
            }
            b"pssh" => validate_pssh(source, view, budget)?,
            kind if is_track_hierarchy_box(kind) => {
                return Err(AdmissionError::InvalidTrackHierarchy);
            }
            _ => {}
        }
        offset = view.end;
    }
    if !movie_header_seen {
        return Err(AdmissionError::MissingRequiredBox(RequiredBox::MovieHeader));
    }
    Ok(())
}

fn scan_track(
    source: &[u8],
    mut offset: usize,
    end: usize,
    budget: &mut ScanBudget,
) -> Result<PreflightTrack, AdmissionError> {
    let mut track_id = None;
    let mut media = None;
    while offset < end {
        budget.inspect_nested_box()?;
        let view = parse_box(source, offset, end)?;
        match &view.kind {
            b"tkhd" => {
                if track_id.is_some() {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::TrackHeader,
                    ));
                }
                if media.is_some() {
                    return Err(AdmissionError::InvalidBoxOrder);
                }
                track_id = Some(parse_track_header_identity(source, view)?);
            }
            b"mdia" => {
                if media.is_some() {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::TrackMedia,
                    ));
                }
                if track_id.is_none() {
                    return Err(AdmissionError::InvalidBoxOrder);
                }
                media = Some(view);
            }
            kind if is_track_hierarchy_box(kind) => {
                return Err(AdmissionError::InvalidTrackHierarchy);
            }
            _ => {}
        }
        offset = view.end;
    }
    let id = track_id.ok_or(AdmissionError::MissingRequiredBox(RequiredBox::TrackHeader))?;
    let media = media.ok_or(AdmissionError::MissingRequiredBox(RequiredBox::TrackMedia))?;
    let (kind, descriptions) = scan_media(source, media.payload_start, media.end, budget)?;
    Ok(PreflightTrack {
        id,
        kind,
        descriptions,
    })
}

fn scan_media(
    source: &[u8],
    mut offset: usize,
    end: usize,
    budget: &mut ScanBudget,
) -> Result<(TrackKind, Vec<PreflightDescription>), AdmissionError> {
    let mut media_header_seen = false;
    let mut handler = None;
    let mut media_information = None;
    while offset < end {
        budget.inspect_nested_box()?;
        let view = parse_box(source, offset, end)?;
        match &view.kind {
            b"mdhd" => {
                if media_header_seen {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::MediaHeader,
                    ));
                }
                if handler.is_some() || media_information.is_some() {
                    return Err(AdmissionError::InvalidBoxOrder);
                }
                validate_media_header(source, view)?;
                media_header_seen = true;
            }
            b"hdlr" => {
                if handler.is_some() {
                    return Err(AdmissionError::DuplicateRequiredBox(RequiredBox::Handler));
                }
                if !media_header_seen || media_information.is_some() {
                    return Err(AdmissionError::InvalidBoxOrder);
                }
                handler = Some(parse_handler(source, view)?);
            }
            b"minf" => {
                if media_information.is_some() {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::MediaInformation,
                    ));
                }
                handler.ok_or(AdmissionError::InvalidBoxOrder)?;
                media_information = Some(view);
            }
            kind if is_track_hierarchy_box(kind) => {
                return Err(AdmissionError::InvalidTrackHierarchy);
            }
            _ => {}
        }
        offset = view.end;
    }
    if !media_header_seen {
        return Err(AdmissionError::MissingRequiredBox(RequiredBox::MediaHeader));
    }
    let kind = handler.ok_or(AdmissionError::MissingRequiredBox(RequiredBox::Handler))?;
    let media_information = media_information.ok_or(AdmissionError::MissingRequiredBox(
        RequiredBox::MediaInformation,
    ))?;
    let descriptions = scan_media_information(
        source,
        media_information.payload_start,
        media_information.end,
        budget,
        kind,
    )?;
    Ok((kind, descriptions))
}

fn scan_media_information(
    source: &[u8],
    mut offset: usize,
    end: usize,
    budget: &mut ScanBudget,
    kind: TrackKind,
) -> Result<Vec<PreflightDescription>, AdmissionError> {
    let mut sample_table = None;
    while offset < end {
        budget.inspect_nested_box()?;
        let view = parse_box(source, offset, end)?;
        match &view.kind {
            b"stbl" => {
                if sample_table.is_some() {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::SampleTable,
                    ));
                }
                sample_table = Some(view);
            }
            child if is_track_hierarchy_box(child) => {
                return Err(AdmissionError::InvalidTrackHierarchy);
            }
            _ => {}
        }
        offset = view.end;
    }
    let sample_table =
        sample_table.ok_or(AdmissionError::MissingRequiredBox(RequiredBox::SampleTable))?;
    scan_sample_table(
        source,
        sample_table.payload_start,
        sample_table.end,
        budget,
        kind,
    )
}

fn scan_sample_table(
    source: &[u8],
    mut offset: usize,
    end: usize,
    budget: &mut ScanBudget,
    kind: TrackKind,
) -> Result<Vec<PreflightDescription>, AdmissionError> {
    let mut sample_descriptions = None;
    while offset < end {
        budget.inspect_nested_box()?;
        let view = parse_box(source, offset, end)?;
        match &view.kind {
            b"stsd" => {
                if sample_descriptions.is_some() {
                    return Err(AdmissionError::DuplicateRequiredBox(
                        RequiredBox::SampleDescription,
                    ));
                }
                sample_descriptions = Some(view);
            }
            child if is_track_hierarchy_box(child) => {
                return Err(AdmissionError::InvalidTrackHierarchy);
            }
            _ => {}
        }
        offset = view.end;
    }
    let sample_descriptions = sample_descriptions.ok_or(AdmissionError::MissingRequiredBox(
        RequiredBox::SampleDescription,
    ))?;
    scan_sample_descriptions(source, sample_descriptions, budget, kind)
}

fn scan_sample_descriptions(
    source: &[u8],
    view: BoxView,
    budget: &mut ScanBudget,
    kind: TrackKind,
) -> Result<Vec<PreflightDescription>, AdmissionError> {
    let (version, flags) = full_box_fields(source, view)?;
    let count_offset = view
        .payload_start
        .checked_add(4)
        .ok_or(AdmissionError::MalformedStructure)?;
    let entries_offset = count_offset
        .checked_add(4)
        .ok_or(AdmissionError::MalformedStructure)?;
    if version != 0 || flags != 0 || entries_offset > view.end {
        return Err(AdmissionError::MalformedStructure);
    }
    let declared = usize::try_from(read_u32(source, count_offset, view.end)?)
        .map_err(|_| AdmissionError::TooManySampleDescriptions)?;
    if declared == 0 {
        return Err(AdmissionError::MissingSampleDescription);
    }
    if declared > MAX_SAMPLE_DESCRIPTIONS_PER_TRACK {
        return Err(AdmissionError::TooManySampleDescriptions);
    }
    let framed = count_sample_description_frames(source, entries_offset, view.end)?;
    if framed != declared {
        return Err(AdmissionError::SampleDescriptionCountMismatch);
    }
    let mut descriptions = Vec::new();
    descriptions
        .try_reserve_exact(declared)
        .map_err(|_| AdmissionError::OutOfMemory)?;
    let mut offset = entries_offset;
    while offset < view.end {
        budget.inspect_nested_box()?;
        let entry = parse_box(source, offset, view.end)?;
        descriptions.push(scan_sample_entry(source, entry, budget, kind)?);
        offset = entry.end;
    }
    if descriptions.len() != framed {
        return Err(AdmissionError::SampleDescriptionCountMismatch);
    }
    Ok(descriptions)
}

fn count_sample_description_frames(
    source: &[u8],
    mut offset: usize,
    end: usize,
) -> Result<usize, AdmissionError> {
    let mut framed = 0usize;
    while offset < end {
        let entry = parse_box(source, offset, end)?;
        framed = framed
            .checked_add(1)
            .ok_or(AdmissionError::TooManySampleDescriptions)?;
        if framed > MAX_SAMPLE_DESCRIPTIONS_PER_TRACK {
            return Err(AdmissionError::TooManySampleDescriptions);
        }
        offset = entry.end;
    }
    Ok(framed)
}

fn scan_sample_entry(
    source: &[u8],
    entry: BoxView,
    budget: &mut ScanBudget,
    track_kind: TrackKind,
) -> Result<PreflightDescription, AdmissionError> {
    let audio = track_kind == TrackKind::Audio;
    let (protected_entry, child_start, bare_configuration) = if audio {
        match &entry.kind {
            b".mp3" => (
                false,
                audio_child_start(source, entry)?,
                Some(PreflightConfiguration {
                    kind: ConfigurationKind::BareMp3,
                    decoder_specific: None,
                }),
            ),
            b"mp4a" => (false, audio_child_start(source, entry)?, None),
            b"enca" => (true, audio_child_start(source, entry)?, None),
            b"avc1" | b"avc3" | b"encv" | b"hvc1" | b"hev1" | b"mp4v" | b"av01" | b"vp08"
            | b"vp09" | b"s263" => {
                return Err(AdmissionError::SampleDescriptionKindMismatch);
            }
            _ => return Err(AdmissionError::UnsupportedCodec),
        }
    } else {
        match &entry.kind {
            b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"mp4v" | b"av01" | b"vp08" | b"vp09"
            | b"s263" => (false, video_child_start(entry)?, None),
            b"encv" => (true, video_child_start(entry)?, None),
            b".mp3" | b"mp4a" | b"enca" => {
                return Err(AdmissionError::SampleDescriptionKindMismatch);
            }
            _ => return Err(AdmissionError::UnsupportedCodec),
        }
    };

    let mut configuration = bare_configuration;
    let mut protection = None;
    let mut offset = child_start;
    while offset < entry.end {
        budget.inspect_nested_box()?;
        let child = parse_box(source, offset, entry.end)?;
        if &child.kind == b"sinf" {
            let next = parse_protection(source, child, budget)?;
            if let Some(existing) = protection {
                return Err(AdmissionError::Protection(if existing == next {
                    ProtectionFailure::Duplicate
                } else {
                    ProtectionFailure::Conflicting
                }));
            }
            protection = Some(next);
        } else {
            let next = classify_configuration(source, child, audio, budget)?;
            if configuration.replace(next).is_some() {
                return Err(AdmissionError::InvalidCodecConfigurationQuantity);
            }
        }
        offset = child.end;
    }

    let configuration = configuration.ok_or(AdmissionError::InvalidCodecConfigurationQuantity)?;
    if protected_entry && protection.is_none() {
        return Err(AdmissionError::Protection(ProtectionFailure::Missing));
    }
    if !protected_entry && protection.is_some() {
        return Err(AdmissionError::Protection(ProtectionFailure::Conflicting));
    }
    if !protected_entry && !configuration_matches_sample_entry(configuration.kind, entry.kind) {
        return Err(AdmissionError::SampleDescriptionKindMismatch);
    }
    if let Some(protection) = protection.as_ref()
        && !configuration_matches_original_format(configuration.kind, protection.original_format)
    {
        return Err(AdmissionError::Protection(ProtectionFailure::CodecMismatch));
    }
    if !matches!(
        configuration.kind,
        ConfigurationKind::BareMp3 | ConfigurationKind::AudioEsDescriptor | ConfigurationKind::Avc
    ) {
        return Err(AdmissionError::UnsupportedCodecConfiguration);
    }
    if audio
        && !matches!(
            configuration.kind,
            ConfigurationKind::BareMp3 | ConfigurationKind::AudioEsDescriptor
        )
        || !audio && configuration.kind != ConfigurationKind::Avc
    {
        return Err(AdmissionError::SampleDescriptionKindMismatch);
    }
    Ok(PreflightDescription {
        configuration: configuration.kind,
        decoder_specific: configuration.decoder_specific,
        protection,
    })
}

fn classify_configuration(
    source: &[u8],
    view: BoxView,
    audio: bool,
    budget: &mut ScanBudget,
) -> Result<PreflightConfiguration, AdmissionError> {
    let bytes = view.end - view.payload_start;
    budget.charge_configuration(bytes)?;
    let (kind, decoder_specific) = match (&view.kind, audio) {
        (b"esds", true) => {
            let (version, flags) = full_box_fields(source, view)?;
            if version != 0 || flags != 0 {
                return Err(AdmissionError::MalformedCodecConfiguration);
            }
            (
                ConfigurationKind::AudioEsDescriptor,
                preflight_audio_es_descriptor(source, view)?,
            )
        }
        (b"avcC", false) => {
            validate_avc_configuration(&source[view.payload_start..view.end])?;
            (ConfigurationKind::Avc, None)
        }
        (b"hvcC", false) => (ConfigurationKind::Hevc, None),
        (b"esds", false) => (ConfigurationKind::Mpeg4Visual, None),
        (b"av1C", false) => (ConfigurationKind::Av1, None),
        (b"vpcC", false) => (ConfigurationKind::Vpx, None),
        (b"d263", false) => (ConfigurationKind::H263, None),
        _ => return Err(AdmissionError::UnsupportedCodec),
    };
    Ok(PreflightConfiguration {
        kind,
        decoder_specific,
    })
}

fn preflight_audio_es_descriptor(
    source: &[u8],
    view: BoxView,
) -> Result<Option<ByteRange>, AdmissionError> {
    const ES_DESCRIPTOR_TAG: u8 = 0x03;

    let mut offset = view
        .payload_start
        .checked_add(4)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    let mut elementary_stream = None;
    while offset < view.end {
        let descriptor = parse_es_descriptor_frame(source, offset, view.end)?;
        if descriptor.tag != ES_DESCRIPTOR_TAG {
            return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
        }
        if let Some(existing) = elementary_stream {
            return Err(AdmissionError::EsDescriptor(
                if descriptor_payloads_equal(source, existing, descriptor)? {
                    EsDescriptorFailure::DuplicateElementaryStream
                } else {
                    EsDescriptorFailure::ConflictingElementaryStream
                },
            ));
        }
        elementary_stream = Some(descriptor);
        offset = descriptor.end;
    }
    let elementary_stream = elementary_stream.ok_or(AdmissionError::EsDescriptor(
        EsDescriptorFailure::MissingElementaryStream,
    ))?;
    preflight_elementary_stream_descriptor(source, elementary_stream)
}

fn preflight_elementary_stream_descriptor(
    source: &[u8],
    descriptor: DescriptorView,
) -> Result<Option<ByteRange>, AdmissionError> {
    const DECODER_CONFIG_TAG: u8 = 0x04;
    const SL_CONFIG_TAG: u8 = 0x06;

    let fixed_end = descriptor
        .payload_start
        .checked_add(3)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    if fixed_end > descriptor.end {
        return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
    }

    let flags = source[descriptor.payload_start + 2];
    let mut offset = fixed_end;
    if flags & 0x80 != 0 {
        offset = advance_es_descriptor_offset(offset, 2, descriptor.end)?;
    }
    if flags & 0x40 != 0 {
        if offset >= descriptor.end || flags & 0x20 == 0 {
            return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
        }
        let url_length = usize::from(source[offset]);
        offset = advance_es_descriptor_offset(offset, 1, descriptor.end)?;
        offset = advance_es_descriptor_offset(offset, url_length, descriptor.end)?;
        offset = advance_es_descriptor_offset(offset, 2, descriptor.end)?;
    } else if flags & 0x20 != 0 {
        // mp4parse 0.17.0 does not consume a standalone OCR_ES_Id. Reject rather than let its
        // descriptor cursor disagree with this exact preflight.
        return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
    }

    let mut decoder_config = None;
    let mut sl_config_seen = false;
    while offset < descriptor.end {
        let child = parse_es_descriptor_frame(source, offset, descriptor.end)?;
        match child.tag {
            DECODER_CONFIG_TAG => {
                if sl_config_seen {
                    return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
                }
                if let Some(existing) = decoder_config {
                    return Err(AdmissionError::EsDescriptor(
                        if descriptor_payloads_equal(source, existing, child)? {
                            EsDescriptorFailure::DuplicateDecoderConfig
                        } else {
                            EsDescriptorFailure::ConflictingDecoderConfig
                        },
                    ));
                }
                decoder_config = Some(child);
            }
            SL_CONFIG_TAG => {
                if decoder_config.is_none() || sl_config_seen || child.payload_start == child.end {
                    return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
                }
                sl_config_seen = true;
            }
            _ => {
                return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
            }
        }
        offset = child.end;
    }
    let decoder_config = decoder_config.ok_or(AdmissionError::EsDescriptor(
        EsDescriptorFailure::MissingDecoderConfig,
    ))?;
    preflight_decoder_config_descriptor(source, decoder_config)
}

fn preflight_decoder_config_descriptor(
    source: &[u8],
    descriptor: DescriptorView,
) -> Result<Option<ByteRange>, AdmissionError> {
    const DECODER_SPECIFIC_TAG: u8 = 0x05;
    const DECODER_CONFIG_FIXED_BYTES: usize = 13;

    let mut offset = descriptor
        .payload_start
        .checked_add(DECODER_CONFIG_FIXED_BYTES)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    if offset > descriptor.end {
        return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
    }
    let object_profile = source[descriptor.payload_start];
    let stream_type = source[descriptor.payload_start + 1];
    if (stream_type >> 2) & 0x3f != 0x05 || stream_type & 0x01 != 0x01 {
        return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
    }
    let aac = matches!(object_profile, 0x40 | 0x66 | 0x67);
    let mp3 = matches!(object_profile, 0x69 | 0x6b);
    if !aac && !mp3 {
        return Err(AdmissionError::UnsupportedCodecConfiguration);
    }

    let mut decoder_specific = None;
    while offset < descriptor.end {
        let child = parse_es_descriptor_frame(source, offset, descriptor.end)?;
        if child.tag != DECODER_SPECIFIC_TAG || child.payload_start == child.end {
            return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
        }
        if let Some(existing) = decoder_specific {
            return Err(AdmissionError::EsDescriptor(
                if descriptor_payloads_equal(source, existing, child)? {
                    EsDescriptorFailure::DuplicateDecoderSpecificInfo
                } else {
                    EsDescriptorFailure::ConflictingDecoderSpecificInfo
                },
            ));
        }
        decoder_specific = Some(child);
        offset = child.end;
    }

    if aac {
        let decoder_specific = decoder_specific.ok_or(AdmissionError::EsDescriptor(
            EsDescriptorFailure::MissingDecoderSpecificInfo,
        ))?;
        Ok(Some(ByteRange {
            start: decoder_specific.payload_start,
            end: decoder_specific.end,
        }))
    } else if decoder_specific.is_some() {
        Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))
    } else {
        Ok(None)
    }
}

fn parse_es_descriptor_frame(
    source: &[u8],
    start: usize,
    enclosing_end: usize,
) -> Result<DescriptorView, AdmissionError> {
    let tag = *source
        .get(start)
        .filter(|_| start < enclosing_end)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    let mut offset = start
        .checked_add(1)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    let mut length = 0usize;
    let mut terminated = false;
    for _ in 0..4 {
        let octet = *source
            .get(offset)
            .filter(|_| offset < enclosing_end)
            .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
        offset = offset
            .checked_add(1)
            .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
        let group = usize::from(octet & 0x7f);
        length = length
            .checked_mul(128)
            .and_then(|value| value.checked_add(group))
            .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
        if octet & 0x80 == 0 {
            terminated = true;
            break;
        }
    }
    if !terminated {
        return Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed));
    }
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= enclosing_end)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    Ok(DescriptorView {
        tag,
        payload_start: offset,
        end,
    })
}

fn advance_es_descriptor_offset(
    offset: usize,
    bytes: usize,
    end: usize,
) -> Result<usize, AdmissionError> {
    offset
        .checked_add(bytes)
        .filter(|next| *next <= end)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))
}

fn descriptor_payloads_equal(
    source: &[u8],
    left: DescriptorView,
    right: DescriptorView,
) -> Result<bool, AdmissionError> {
    let left = source
        .get(left.payload_start..left.end)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    let right = source
        .get(right.payload_start..right.end)
        .ok_or(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))?;
    Ok(left == right)
}

fn configuration_matches_original_format(kind: ConfigurationKind, format: [u8; 4]) -> bool {
    match kind {
        ConfigurationKind::BareMp3 => &format == b".mp3",
        ConfigurationKind::AudioEsDescriptor => matches!(&format, b"mp4a" | b".mp3"),
        ConfigurationKind::Avc => matches!(&format, b"avc1" | b"avc3"),
        ConfigurationKind::Hevc => matches!(&format, b"hev1" | b"hvc1"),
        ConfigurationKind::Mpeg4Visual => &format == b"mp4v",
        ConfigurationKind::Av1 => &format == b"av01",
        ConfigurationKind::Vpx => matches!(&format, b"vp08" | b"vp09"),
        ConfigurationKind::H263 => &format == b"s263",
    }
}

fn configuration_matches_sample_entry(kind: ConfigurationKind, entry: [u8; 4]) -> bool {
    match kind {
        ConfigurationKind::BareMp3 => &entry == b".mp3",
        ConfigurationKind::AudioEsDescriptor => &entry == b"mp4a",
        ConfigurationKind::Avc => matches!(&entry, b"avc1" | b"avc3"),
        ConfigurationKind::Hevc => matches!(&entry, b"hev1" | b"hvc1"),
        ConfigurationKind::Mpeg4Visual => &entry == b"mp4v",
        ConfigurationKind::Av1 => &entry == b"av01",
        ConfigurationKind::Vpx => matches!(&entry, b"vp08" | b"vp09"),
        ConfigurationKind::H263 => &entry == b"s263",
    }
}

fn parse_protection(
    source: &[u8],
    view: BoxView,
    budget: &mut ScanBudget,
) -> Result<PreflightProtection, AdmissionError> {
    let mut original_format = None;
    let mut scheme = None;
    let mut encryption = None;
    let mut offset = view.payload_start;
    while offset < view.end {
        budget.inspect_nested_box()?;
        let child = parse_box(source, offset, view.end)?;
        match &child.kind {
            b"frma" => {
                let value = read_fourcc_payload(source, child)?;
                set_protection_singleton(&mut original_format, value)?;
            }
            b"schm" => {
                let value = parse_scheme(source, child)?;
                set_protection_singleton(&mut scheme, value)?;
            }
            b"schi" => {
                let value = parse_scheme_information(source, child, budget)?;
                set_protection_singleton(&mut encryption, value)?;
            }
            _ => return Err(AdmissionError::Protection(ProtectionFailure::Incomplete)),
        }
        offset = child.end;
    }
    let original_format =
        original_format.ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))?;
    let (scheme_type, scheme_version) =
        scheme.ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))?;
    if !matches!(&scheme_type, b"cenc" | b"cens" | b"cbc1" | b"cbcs") {
        return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
    }
    let encryption = encryption.ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))?;
    Ok(PreflightProtection {
        original_format,
        scheme_type,
        scheme_version,
        is_encrypted: encryption.is_encrypted,
        iv_size: encryption.iv_size,
        kid: encryption.kid,
        constant_iv_size: encryption.constant_iv_size,
        constant_iv: encryption.constant_iv,
    })
}

fn set_protection_singleton<T: Copy + Eq>(
    destination: &mut Option<T>,
    value: T,
) -> Result<(), AdmissionError> {
    if let Some(existing) = destination {
        return Err(AdmissionError::Protection(if *existing == value {
            ProtectionFailure::Duplicate
        } else {
            ProtectionFailure::Conflicting
        }));
    }
    *destination = Some(value);
    Ok(())
}

fn parse_scheme(source: &[u8], view: BoxView) -> Result<([u8; 4], u32), AdmissionError> {
    let (version, flags) = full_box_fields(source, view)?;
    if version != 0 || flags != 0 || view.end - view.payload_start != 12 {
        return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
    }
    Ok((
        read_fourcc(source, view.payload_start + 4, view.end)?,
        read_u32(source, view.payload_start + 8, view.end)?,
    ))
}

fn parse_scheme_information(
    source: &[u8],
    view: BoxView,
    budget: &mut ScanBudget,
) -> Result<PreflightEncryption, AdmissionError> {
    let mut encryption = None;
    let mut offset = view.payload_start;
    while offset < view.end {
        budget.inspect_nested_box()?;
        let child = parse_box(source, offset, view.end)?;
        if &child.kind != b"tenc" {
            return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
        }
        let value = parse_track_encryption(source, child)?;
        set_protection_singleton(&mut encryption, value)?;
        offset = child.end;
    }
    encryption.ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))
}

fn parse_track_encryption(
    source: &[u8],
    view: BoxView,
) -> Result<PreflightEncryption, AdmissionError> {
    let (version, flags) = full_box_fields(source, view)?;
    let base_end = view
        .payload_start
        .checked_add(24)
        .ok_or(AdmissionError::MalformedStructure)?;
    if version > 1 || flags != 0 || base_end > view.end || source[view.payload_start + 4] != 0 {
        return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
    }
    if version == 0 && source[view.payload_start + 5] != 0 {
        return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
    }
    let is_encrypted = source[view.payload_start + 6];
    let iv_size = source[view.payload_start + 7];
    let mut kid = [0u8; 16];
    kid.copy_from_slice(&source[view.payload_start + 8..base_end]);
    let mut constant_iv = [0u8; 16];
    let constant_iv_size = if is_encrypted == 1 && iv_size == 0 {
        let size = *source
            .get(base_end)
            .ok_or(AdmissionError::Protection(ProtectionFailure::Incomplete))?;
        let iv_end = base_end
            .checked_add(1 + usize::from(size))
            .ok_or(AdmissionError::MalformedStructure)?;
        if !matches!(size, 8 | 16) || iv_end != view.end {
            return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
        }
        constant_iv[..usize::from(size)].copy_from_slice(&source[base_end + 1..iv_end]);
        Some(size)
    } else {
        if base_end != view.end {
            return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
        }
        None
    };
    if is_encrypted != 1 || (!matches!(iv_size, 8 | 16) && constant_iv_size.is_none()) {
        return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
    }
    Ok(PreflightEncryption {
        is_encrypted,
        iv_size,
        kid,
        constant_iv_size,
        constant_iv,
    })
}

fn validate_pssh(
    source: &[u8],
    view: BoxView,
    budget: &mut ScanBudget,
) -> Result<(), AdmissionError> {
    let bytes = view.end - view.payload_start;
    if bytes > MAX_PSSH_BYTES || budget.protection_system_headers >= MAX_PSSH_BOXES {
        return Err(AdmissionError::ProtectionSystemHeaderPolicyExceeded);
    }
    let (version, flags) = full_box_fields(source, view)?;
    if version > 1 || flags != 0 {
        return Err(AdmissionError::MalformedStructure);
    }
    let mut position = view
        .payload_start
        .checked_add(20)
        .ok_or(AdmissionError::MalformedStructure)?;
    if position > view.end {
        return Err(AdmissionError::MalformedStructure);
    }
    if version == 1 {
        let count = usize::try_from(read_u32(source, position, view.end)?)
            .map_err(|_| AdmissionError::ProtectionSystemHeaderPolicyExceeded)?;
        if count > MAX_PSSH_KEY_IDS {
            return Err(AdmissionError::ProtectionSystemHeaderPolicyExceeded);
        }
        position = position
            .checked_add(4)
            .and_then(|value| value.checked_add(count.checked_mul(16)?))
            .ok_or(AdmissionError::MalformedStructure)?;
        if position > view.end {
            return Err(AdmissionError::MalformedStructure);
        }
    }
    let data_length = usize::try_from(read_u32(source, position, view.end)?)
        .map_err(|_| AdmissionError::ProtectionSystemHeaderPolicyExceeded)?;
    position = position
        .checked_add(4)
        .and_then(|value| value.checked_add(data_length))
        .ok_or(AdmissionError::MalformedStructure)?;
    if position != view.end {
        return Err(AdmissionError::MalformedStructure);
    }
    budget.protection_system_headers += 1;
    budget.protection_system_header_bytes = budget
        .protection_system_header_bytes
        .checked_add(bytes)
        .ok_or(AdmissionError::ProtectionSystemHeaderPolicyExceeded)?;
    if budget.protection_system_header_bytes > MAX_TOTAL_PSSH_BYTES {
        return Err(AdmissionError::ProtectionSystemHeaderPolicyExceeded);
    }
    Ok(())
}

fn validate_movie_header(source: &[u8], view: BoxView) -> Result<(), AdmissionError> {
    let (version, _) = full_box_fields(source, view)?;
    let (expected_length, timescale_offset) = match version {
        0 => (100, view.payload_start + 12),
        1 => (112, view.payload_start + 20),
        _ => return Err(AdmissionError::MalformedStructure),
    };
    if view.end - view.payload_start != expected_length {
        return Err(AdmissionError::MalformedStructure);
    }
    if read_u32(source, timescale_offset, view.end)? == 0 {
        return Err(AdmissionError::InvalidMovieTimescale);
    }
    Ok(())
}

fn parse_track_header_identity(source: &[u8], view: BoxView) -> Result<u32, AdmissionError> {
    let (version, _) = full_box_fields(source, view)?;
    let (expected_length, identity_offset) = match version {
        0 => (84, view.payload_start + 12),
        1 => (96, view.payload_start + 20),
        _ => return Err(AdmissionError::MalformedStructure),
    };
    if view.end - view.payload_start != expected_length {
        return Err(AdmissionError::MalformedStructure);
    }
    let identity = read_u32(source, identity_offset, view.end)?;
    if identity == 0 {
        return Err(AdmissionError::InvalidTrackIdentity);
    }
    Ok(identity)
}

fn validate_media_header(source: &[u8], view: BoxView) -> Result<(), AdmissionError> {
    let (version, _) = full_box_fields(source, view)?;
    let (expected_length, timescale_offset) = match version {
        0 => (24, view.payload_start + 12),
        1 => (36, view.payload_start + 20),
        _ => return Err(AdmissionError::MalformedStructure),
    };
    if view.end - view.payload_start != expected_length {
        return Err(AdmissionError::MalformedStructure);
    }
    if read_u32(source, timescale_offset, view.end)? == 0 {
        return Err(AdmissionError::InvalidTrackTimescale);
    }
    Ok(())
}

fn parse_handler(source: &[u8], view: BoxView) -> Result<TrackKind, AdmissionError> {
    const FIXED_HANDLER_BYTES: usize = 24;
    const MAX_HANDLER_NAME_BYTES: usize = 1_024;
    let (version, flags) = full_box_fields(source, view)?;
    let payload_length = view.end - view.payload_start;
    if version != 0
        || flags != 0
        || !(FIXED_HANDLER_BYTES + 1..=FIXED_HANDLER_BYTES + MAX_HANDLER_NAME_BYTES)
            .contains(&payload_length)
        || source[view.end - 1] != 0
    {
        return Err(AdmissionError::MalformedStructure);
    }
    match &read_fourcc(source, view.payload_start + 8, view.end)? {
        b"soun" => Ok(TrackKind::Audio),
        b"vide" => Ok(TrackKind::Video),
        b"pict" => Ok(TrackKind::Picture),
        b"auxv" => Ok(TrackKind::AuxiliaryVideo),
        _ => Err(AdmissionError::UnsupportedTrackKind),
    }
}

fn audio_child_start(source: &[u8], entry: BoxView) -> Result<usize, AdmissionError> {
    let child_start = entry
        .payload_start
        .checked_add(28)
        .ok_or(AdmissionError::MalformedStructure)?;
    if child_start > entry.end || read_u16(source, entry.payload_start + 8, entry.end)? != 0 {
        return Err(AdmissionError::MalformedStructure);
    }
    Ok(child_start)
}

fn video_child_start(entry: BoxView) -> Result<usize, AdmissionError> {
    let child_start = entry
        .payload_start
        .checked_add(78)
        .ok_or(AdmissionError::MalformedStructure)?;
    if child_start > entry.end {
        return Err(AdmissionError::MalformedStructure);
    }
    Ok(child_start)
}

fn full_box_fields(source: &[u8], view: BoxView) -> Result<(u8, u32), AdmissionError> {
    let end = view
        .payload_start
        .checked_add(4)
        .ok_or(AdmissionError::MalformedStructure)?;
    if end > view.end {
        return Err(AdmissionError::MalformedStructure);
    }
    Ok((
        source[view.payload_start],
        u32::from_be_bytes([
            0,
            source[view.payload_start + 1],
            source[view.payload_start + 2],
            source[view.payload_start + 3],
        ]),
    ))
}

fn read_fourcc_payload(source: &[u8], view: BoxView) -> Result<[u8; 4], AdmissionError> {
    if view.end - view.payload_start != 4 {
        return Err(AdmissionError::Protection(ProtectionFailure::Incomplete));
    }
    read_fourcc(source, view.payload_start, view.end)
}

fn read_fourcc(source: &[u8], offset: usize, end: usize) -> Result<[u8; 4], AdmissionError> {
    let value_end = offset
        .checked_add(4)
        .ok_or(AdmissionError::MalformedStructure)?;
    if value_end > end || end > source.len() {
        return Err(AdmissionError::MalformedStructure);
    }
    Ok([
        source[offset],
        source[offset + 1],
        source[offset + 2],
        source[offset + 3],
    ])
}

fn read_u32(source: &[u8], offset: usize, end: usize) -> Result<u32, AdmissionError> {
    Ok(u32::from_be_bytes(read_fourcc(source, offset, end)?))
}

fn read_u16(source: &[u8], offset: usize, end: usize) -> Result<u16, AdmissionError> {
    let value_end = offset
        .checked_add(2)
        .ok_or(AdmissionError::MalformedStructure)?;
    if value_end > end || end > source.len() {
        return Err(AdmissionError::MalformedStructure);
    }
    Ok(u16::from_be_bytes([source[offset], source[offset + 1]]))
}

fn is_track_hierarchy_box(kind: &[u8; 4]) -> bool {
    matches!(
        kind,
        b"mvhd" | b"trak" | b"tkhd" | b"mdia" | b"mdhd" | b"hdlr" | b"minf" | b"stbl" | b"stsd"
    )
}

fn parse_brands(payload: &[u8]) -> Result<ContainerBrands, AdmissionError> {
    if payload.len() < 8 || !(payload.len() - 8).is_multiple_of(4) {
        return Err(AdmissionError::MalformedStructure);
    }
    let compatible_count = (payload.len() - 8) / 4;
    if compatible_count > MAX_COMPATIBLE_BRANDS {
        return Err(AdmissionError::TooManyCompatibleBrands);
    }
    let mut compatible = Vec::new();
    compatible
        .try_reserve_exact(compatible_count)
        .map_err(|_| AdmissionError::OutOfMemory)?;
    for bytes in payload[8..].chunks_exact(4) {
        compatible.push(FourCc([bytes[0], bytes[1], bytes[2], bytes[3]]));
    }
    Ok(ContainerBrands {
        major: FourCc([payload[0], payload[1], payload[2], payload[3]]),
        compatible,
    })
}

fn parse_box(source: &[u8], start: usize, enclosing_end: usize) -> Result<BoxView, AdmissionError> {
    let short_header_end = start
        .checked_add(8)
        .ok_or(AdmissionError::MalformedStructure)?;
    if short_header_end > enclosing_end || enclosing_end > source.len() {
        return Err(AdmissionError::MalformedStructure);
    }
    let short_size = u32::from_be_bytes([
        source[start],
        source[start + 1],
        source[start + 2],
        source[start + 3],
    ]);
    let kind = [
        source[start + 4],
        source[start + 5],
        source[start + 6],
        source[start + 7],
    ];
    let (size, payload_start) = match short_size {
        0 => return Err(AdmissionError::MalformedStructure),
        1 => {
            let extended_end = start
                .checked_add(16)
                .ok_or(AdmissionError::MalformedStructure)?;
            if extended_end > enclosing_end {
                return Err(AdmissionError::MalformedStructure);
            }
            let size = u64::from_be_bytes([
                source[start + 8],
                source[start + 9],
                source[start + 10],
                source[start + 11],
                source[start + 12],
                source[start + 13],
                source[start + 14],
                source[start + 15],
            ]);
            (size, extended_end)
        }
        value => (u64::from(value), short_header_end),
    };
    let header_len = payload_start - start;
    let size = usize::try_from(size).map_err(|_| AdmissionError::MalformedStructure)?;
    if size < header_len {
        return Err(AdmissionError::MalformedStructure);
    }
    let end = start
        .checked_add(size)
        .ok_or(AdmissionError::MalformedStructure)?;
    if end > enclosing_end {
        return Err(AdmissionError::MalformedStructure);
    }
    Ok(BoxView {
        kind,
        payload_start,
        end,
    })
}
