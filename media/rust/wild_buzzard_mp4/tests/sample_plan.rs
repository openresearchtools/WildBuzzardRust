use wild_buzzard_mp4::{
    MAX_PLANNED_SAMPLES_PER_TRACK, MAX_SAMPLE_TABLE_RUNS, SampleCardinality, SamplePlanError,
    SamplePlanResource, SampleTable, admit_complete_mp4, plan_non_fragmented_track_samples,
};

fn boxed(kind: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let size = u32::try_from(payload.len() + 8).unwrap();
    let mut result = Vec::with_capacity(size as usize);
    result.extend_from_slice(&size.to_be_bytes());
    result.extend_from_slice(kind);
    result.extend_from_slice(&payload);
    result
}

fn full_box(version: u8, flags: u32, mut payload: Vec<u8>) -> Vec<u8> {
    let mut result = Vec::with_capacity(payload.len() + 4);
    result.push(version);
    result.extend_from_slice(&flags.to_be_bytes()[1..]);
    result.append(&mut payload);
    result
}

fn container(kind: &[u8; 4], children: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut payload = Vec::new();
    for child in children {
        payload.extend_from_slice(&child);
    }
    boxed(kind, payload)
}

fn ftyp() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"isom");
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(b"isom");
    payload.extend_from_slice(b"iso2");
    boxed(b"ftyp", payload)
}

fn mvhd() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&1_000u32.to_be_bytes());
    payload.extend_from_slice(&5_000u32.to_be_bytes());
    payload.extend_from_slice(&[0; 80]);
    boxed(b"mvhd", full_box(0, 0, payload))
}

fn tkhd(track_id: u32, width: u16, height: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&track_id.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&5_000u32.to_be_bytes());
    payload.extend_from_slice(&[0; 16]);
    for value in [1i32 << 16, 0, 0, 0, 1i32 << 16, 0, 0, 0, 1i32 << 30] {
        payload.extend_from_slice(&value.to_be_bytes());
    }
    payload.extend_from_slice(&(u32::from(width) << 16).to_be_bytes());
    payload.extend_from_slice(&(u32::from(height) << 16).to_be_bytes());
    boxed(b"tkhd", full_box(0, 3, payload))
}

fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&timescale.to_be_bytes());
    payload.extend_from_slice(&duration.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    boxed(b"mdhd", full_box(0, 0, payload))
}

fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(kind);
    payload.extend_from_slice(&[0; 12]);
    payload.push(0);
    boxed(b"hdlr", full_box(0, 0, payload))
}

fn video_entry() -> Vec<u8> {
    let mut config = vec![1, 66, 0, 30, 0xff, 0xe1];
    config.extend_from_slice(&3u16.to_be_bytes());
    config.extend_from_slice(&[0x67, 0x42, 0]);
    config.push(1);
    config.extend_from_slice(&2u16.to_be_bytes());
    config.extend_from_slice(&[0x68, 0]);

    let mut payload = vec![0; 6];
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&[0; 16]);
    payload.extend_from_slice(&640u16.to_be_bytes());
    payload.extend_from_slice(&360u16.to_be_bytes());
    payload.extend_from_slice(&[0; 50]);
    payload.extend_from_slice(&boxed(b"avcC", config));
    boxed(b"avc1", payload)
}

fn audio_entry() -> Vec<u8> {
    let mut payload = vec![0; 6];
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&[0; 6]);
    payload.extend_from_slice(&2u16.to_be_bytes());
    payload.extend_from_slice(&16u16.to_be_bytes());
    payload.extend_from_slice(&[0; 4]);
    payload.extend_from_slice(&(48_000u32 << 16).to_be_bytes());
    boxed(b".mp3", payload)
}

fn stsd(entry: Vec<u8>) -> Vec<u8> {
    let mut payload = 1u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&entry);
    boxed(b"stsd", full_box(0, 0, payload))
}

fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut payload = u32::try_from(runs.len()).unwrap().to_be_bytes().to_vec();
    for &(count, delta) in runs {
        payload.extend_from_slice(&count.to_be_bytes());
        payload.extend_from_slice(&delta.to_be_bytes());
    }
    boxed(b"stts", full_box(0, 0, payload))
}

fn ctts(version: u8, runs: &[(u32, i32)]) -> Vec<u8> {
    let mut payload = u32::try_from(runs.len()).unwrap().to_be_bytes().to_vec();
    for &(count, offset) in runs {
        payload.extend_from_slice(&count.to_be_bytes());
        payload.extend_from_slice(&offset.to_be_bytes());
    }
    boxed(b"ctts", full_box(version, 0, payload))
}

fn stsc(runs: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut payload = u32::try_from(runs.len()).unwrap().to_be_bytes().to_vec();
    for &(first_chunk, samples_per_chunk, description_index) in runs {
        payload.extend_from_slice(&first_chunk.to_be_bytes());
        payload.extend_from_slice(&samples_per_chunk.to_be_bytes());
        payload.extend_from_slice(&description_index.to_be_bytes());
    }
    boxed(b"stsc", full_box(0, 0, payload))
}

fn stsz_variable(sizes: &[u32]) -> Vec<u8> {
    let mut payload = 0u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&u32::try_from(sizes.len()).unwrap().to_be_bytes());
    for &size in sizes {
        payload.extend_from_slice(&size.to_be_bytes());
    }
    boxed(b"stsz", full_box(0, 0, payload))
}

fn stsz_constant(size: u32, count: u32) -> Vec<u8> {
    let mut payload = size.to_be_bytes().to_vec();
    payload.extend_from_slice(&count.to_be_bytes());
    boxed(b"stsz", full_box(0, 0, payload))
}

fn stz2_4_bit(sizes: &[u8]) -> Vec<u8> {
    assert!(sizes.iter().all(|size| *size <= 15));
    let mut payload = vec![0, 0, 0, 4];
    payload.extend_from_slice(&u32::try_from(sizes.len()).unwrap().to_be_bytes());
    for pair in sizes.chunks(2) {
        payload.push((pair[0] << 4) | pair.get(1).copied().unwrap_or(0));
    }
    boxed(b"stz2", full_box(0, 0, payload))
}

fn stco(offsets: &[u64]) -> Vec<u8> {
    let mut payload = u32::try_from(offsets.len()).unwrap().to_be_bytes().to_vec();
    for &offset in offsets {
        payload.extend_from_slice(&u32::try_from(offset).unwrap().to_be_bytes());
    }
    boxed(b"stco", full_box(0, 0, payload))
}

fn co64(offsets: &[u64]) -> Vec<u8> {
    let mut payload = u32::try_from(offsets.len()).unwrap().to_be_bytes().to_vec();
    for &offset in offsets {
        payload.extend_from_slice(&offset.to_be_bytes());
    }
    boxed(b"co64", full_box(0, 0, payload))
}

fn stss(samples: &[u32]) -> Vec<u8> {
    let mut payload = u32::try_from(samples.len()).unwrap().to_be_bytes().to_vec();
    for &sample in samples {
        payload.extend_from_slice(&sample.to_be_bytes());
    }
    boxed(b"stss", full_box(0, 0, payload))
}

fn track(track_id: u32, handler: &[u8; 4], entry: Vec<u8>, tables: Vec<Vec<u8>>) -> Vec<u8> {
    let (width, height, timescale) = if handler == b"soun" {
        (0, 0, 48_000)
    } else {
        (640, 360, 90_000)
    };
    let sample_table = container(b"stbl", std::iter::once(stsd(entry)).chain(tables));
    let media = container(
        b"mdia",
        [
            mdhd(timescale, timescale * 5),
            hdlr(handler),
            container(b"minf", [sample_table]),
        ],
    );
    container(b"trak", [tkhd(track_id, width, height), media])
}

fn movie(tracks: Vec<Vec<u8>>, extensions: Vec<Vec<u8>>) -> Vec<u8> {
    let mut children = Vec::with_capacity(1 + tracks.len() + extensions.len());
    children.push(mvhd());
    children.extend(tracks);
    children.extend(extensions);
    boxed(b"moov", children.into_iter().flatten().collect())
}

#[derive(Clone)]
enum OffsetSpec {
    Relative32(Vec<u64>),
    Relative64(Vec<u64>),
    Absolute64(Vec<u64>),
}

impl OffsetSpec {
    fn table(&self, media_data_start: u64) -> Vec<u8> {
        match self {
            Self::Relative32(offsets) => stco(
                &offsets
                    .iter()
                    .map(|offset| media_data_start + offset)
                    .collect::<Vec<_>>(),
            ),
            Self::Relative64(offsets) => co64(
                &offsets
                    .iter()
                    .map(|offset| media_data_start + offset)
                    .collect::<Vec<_>>(),
            ),
            Self::Absolute64(offsets) => co64(offsets),
        }
    }
}

struct Fixture {
    size_table: Vec<u8>,
    decode_runs: Vec<(u32, u32)>,
    composition_runs: Option<(u8, Vec<(u32, i32)>)>,
    chunk_runs: Vec<(u32, u32, u32)>,
    offsets: OffsetSpec,
    sync_samples: Option<Vec<u32>>,
    payload: Vec<u8>,
    movie_extensions: Vec<Vec<u8>>,
}

impl Fixture {
    fn valid_three_samples() -> Self {
        Self {
            size_table: stsz_variable(&[2, 3, 4]),
            decode_runs: vec![(2, 100), (1, 200)],
            composition_runs: Some((1, vec![(1, 10), (2, -20)])),
            chunk_runs: vec![(1, 2, 1), (2, 1, 1)],
            offsets: OffsetSpec::Relative32(vec![0, 5]),
            sync_samples: Some(vec![1, 3]),
            payload: vec![1, 1, 2, 2, 2, 3, 3, 3, 3],
            movie_extensions: Vec::new(),
        }
    }

    fn tables(&self, media_data_start: u64) -> Vec<Vec<u8>> {
        let mut tables = vec![stts(&self.decode_runs)];
        if let Some((version, runs)) = &self.composition_runs {
            tables.push(ctts(*version, runs));
        }
        tables.push(stsc(&self.chunk_runs));
        tables.push(self.size_table.clone());
        tables.push(self.offsets.table(media_data_start));
        if let Some(samples) = &self.sync_samples {
            tables.push(stss(samples));
        }
        tables
    }

    fn build(&self) -> Vec<u8> {
        let file_type = ftyp();
        let dummy_movie = movie(
            vec![track(1, b"vide", video_entry(), self.tables(0))],
            self.movie_extensions.clone(),
        );
        let media_data_start = u64::try_from(file_type.len() + dummy_movie.len() + 8).unwrap();
        let final_movie = movie(
            vec![track(
                1,
                b"vide",
                video_entry(),
                self.tables(media_data_start),
            )],
            self.movie_extensions.clone(),
        );
        assert_eq!(dummy_movie.len(), final_movie.len());
        let mut source = file_type;
        source.extend_from_slice(&final_movie);
        source.extend_from_slice(&boxed(b"mdat", self.payload.clone()));
        source
    }
}

fn interleaved_source() -> Vec<u8> {
    let file_type = ftyp();
    let dummy_video = track(
        1,
        b"vide",
        video_entry(),
        vec![
            stts(&[(2, 100), (1, 200)]),
            ctts(1, &[(1, 10), (2, -20)]),
            stsc(&[(1, 2, 1), (2, 1, 1)]),
            stsz_variable(&[3, 4, 5]),
            stco(&[0, 0]),
            stss(&[1, 3]),
        ],
    );
    let dummy_audio = track(
        2,
        b"soun",
        audio_entry(),
        vec![
            stts(&[(5, 1_024)]),
            stsc(&[(1, 3, 1), (2, 2, 1)]),
            stsz_constant(2, 5),
            co64(&[0, 0]),
        ],
    );
    let dummy_movie = movie(vec![dummy_video, dummy_audio], Vec::new());
    let start = u64::try_from(file_type.len() + dummy_movie.len() + 8).unwrap();
    let video = track(
        1,
        b"vide",
        video_entry(),
        vec![
            stts(&[(2, 100), (1, 200)]),
            ctts(1, &[(1, 10), (2, -20)]),
            stsc(&[(1, 2, 1), (2, 1, 1)]),
            stsz_variable(&[3, 4, 5]),
            stco(&[start, start + 13]),
            stss(&[1, 3]),
        ],
    );
    let audio = track(
        2,
        b"soun",
        audio_entry(),
        vec![
            stts(&[(5, 1_024)]),
            stsc(&[(1, 3, 1), (2, 2, 1)]),
            stsz_constant(2, 5),
            co64(&[start + 7, start + 18]),
        ],
    );
    let final_movie = movie(vec![video, audio], Vec::new());
    assert_eq!(dummy_movie.len(), final_movie.len());

    let mut payload = Vec::new();
    payload.extend_from_slice(&[0x11; 3]);
    payload.extend_from_slice(&[0x12; 4]);
    payload.extend_from_slice(&[0x21; 2]);
    payload.extend_from_slice(&[0x22; 2]);
    payload.extend_from_slice(&[0x23; 2]);
    payload.extend_from_slice(&[0x13; 5]);
    payload.extend_from_slice(&[0x24; 2]);
    payload.extend_from_slice(&[0x25; 2]);

    let mut source = file_type;
    source.extend_from_slice(&final_movie);
    source.extend_from_slice(&boxed(b"mdat", payload));
    source
}

fn admitted_plan(source: &[u8]) -> Result<wild_buzzard_mp4::TrackSamplePlan<'_>, SamplePlanError> {
    let initialization = admit_complete_mp4(source).unwrap();
    let identity = initialization.tracks[0].identity();
    plan_non_fragmented_track_samples(source, &initialization, identity)
}

#[test]
fn plans_interleaved_variable_video_and_constant_audio_without_payload_copies() {
    let source = interleaved_source();
    let initialization = admit_complete_mp4(&source).unwrap();

    let video = plan_non_fragmented_track_samples(
        &source,
        &initialization,
        initialization.tracks[0].identity(),
    )
    .unwrap();
    let video_samples = video.samples();
    assert_eq!(video_samples.len(), 3);
    assert_eq!(
        video_samples
            .iter()
            .map(|sample| sample.byte_range.len())
            .collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(
        video_samples
            .iter()
            .map(|sample| sample.decode_timestamp.units)
            .collect::<Vec<_>>(),
        [0, 100, 200]
    );
    assert_eq!(
        video_samples
            .iter()
            .map(|sample| sample.composition_timestamp.units)
            .collect::<Vec<_>>(),
        [10, 80, 180]
    );
    assert_eq!(
        video_samples
            .iter()
            .map(|sample| sample.duration.units)
            .collect::<Vec<_>>(),
        [100, 100, 200]
    );
    assert_eq!(
        video_samples
            .iter()
            .map(|sample| sample.is_sync)
            .collect::<Vec<_>>(),
        [true, false, true]
    );
    assert!(
        video_samples
            .iter()
            .all(|sample| sample.sample_description_index == 1)
    );
    assert_eq!(video.sample_bytes(0), Some(&[0x11; 3][..]));
    let first_offset = usize::try_from(video_samples[0].byte_range.start()).unwrap();
    assert_eq!(
        video.sample_bytes(0).unwrap().as_ptr(),
        source[first_offset..].as_ptr()
    );

    let audio = plan_non_fragmented_track_samples(
        &source,
        &initialization,
        initialization.tracks[1].identity(),
    )
    .unwrap();
    assert_eq!(audio.samples().len(), 5);
    assert!(audio.samples().iter().all(|sample| sample.is_sync));
    assert!(
        audio
            .samples()
            .iter()
            .all(|sample| sample.byte_range.len() == 2)
    );
    assert_eq!(audio.sample_bytes(0), Some(&[0x21; 2][..]));
    assert_eq!(audio.sample_bytes(3), Some(&[0x24; 2][..]));
    assert_eq!(audio.accounting().chunks, 2);
    assert_eq!(audio.accounting().media_data_boxes, 1);
}

#[test]
fn supports_compact_four_bit_sizes_and_default_sync() {
    let sizes = [1u8, 2, 3, 4, 5];
    let fixture = Fixture {
        size_table: stz2_4_bit(&sizes),
        decode_runs: vec![(5, 1)],
        composition_runs: None,
        chunk_runs: vec![(1, 5, 1)],
        offsets: OffsetSpec::Relative64(vec![0]),
        sync_samples: None,
        payload: (0..15).collect(),
        movie_extensions: Vec::new(),
    };
    let source = fixture.build();
    let plan = admitted_plan(&source).unwrap();
    assert_eq!(
        plan.samples()
            .iter()
            .map(|sample| sample.byte_range.len())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    assert!(plan.samples().iter().all(|sample| sample.is_sync));
    assert_eq!(plan.sample_bytes(4), Some(&[10, 11, 12, 13, 14][..]));
}

#[test]
fn source_and_track_seals_reject_cross_admission_use() {
    let source = interleaved_source();
    let first = admit_complete_mp4(&source).unwrap();
    let identity = first.tracks[0].identity();

    let identical_copy = source.clone();
    assert_eq!(
        plan_non_fragmented_track_samples(&identical_copy, &first, identity.clone()),
        Err(SamplePlanError::SourceBindingMismatch)
    );

    let mut mutable_source = source.clone();
    let mutable_initialization = admit_complete_mp4(&mutable_source).unwrap();
    let mutable_identity = mutable_initialization.tracks[0].identity();
    *mutable_source.last_mut().unwrap() ^= 1;
    assert_eq!(
        plan_non_fragmented_track_samples(
            &mutable_source,
            &mutable_initialization,
            mutable_identity.clone(),
        ),
        Err(SamplePlanError::SourceBindingMismatch)
    );
    *mutable_source.last_mut().unwrap() ^= 1;
    assert!(
        plan_non_fragmented_track_samples(
            &mutable_source,
            &mutable_initialization,
            mutable_identity,
        )
        .is_ok()
    );

    let mut changed = source.clone();
    *changed.last_mut().unwrap() ^= 1;
    let second = admit_complete_mp4(&changed).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        plan_non_fragmented_track_samples(&changed, &first, identity.clone()),
        Err(SamplePlanError::SourceBindingMismatch)
    );
    assert_eq!(
        plan_non_fragmented_track_samples(&source, &first, second.tracks[0].identity()),
        Err(SamplePlanError::SourceBindingMismatch)
    );
    assert!(plan_non_fragmented_track_samples(&source, &first, identity).is_ok());
}

#[test]
fn rejects_timing_chunk_and_sync_cardinality_then_recovers() {
    let mut decode = Fixture::valid_three_samples();
    decode.decode_runs = vec![(2, 100)];
    assert_eq!(
        admitted_plan(&decode.build()),
        Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::DecodeTime
        ))
    );

    let mut composition = Fixture::valid_three_samples();
    composition.composition_runs = Some((0, vec![(2, 0)]));
    assert_eq!(
        admitted_plan(&composition.build()),
        Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::CompositionTime
        ))
    );

    let mut chunks = Fixture::valid_three_samples();
    chunks.chunk_runs = vec![(1, 1, 1), (2, 1, 1)];
    assert_eq!(
        admitted_plan(&chunks.build()),
        Err(SamplePlanError::CardinalityMismatch(
            SampleCardinality::ChunkMapping
        ))
    );

    let mut sync = Fixture::valid_three_samples();
    sync.sync_samples = Some(vec![1, 4]);
    assert_eq!(
        admitted_plan(&sync.build()),
        Err(SamplePlanError::MalformedTable(SampleTable::SyncSample))
    );

    let mut description = Fixture::valid_three_samples();
    description.chunk_runs = vec![(1, 2, 2), (2, 1, 2)];
    assert_eq!(
        admitted_plan(&description.build()),
        Err(SamplePlanError::InvalidSampleDescriptionIndex)
    );

    assert_eq!(
        admitted_plan(&Fixture::valid_three_samples().build())
            .unwrap()
            .samples()
            .len(),
        3
    );
}

#[test]
fn rejects_overflow_source_escape_and_non_mdat_ranges_then_recovers() {
    let mut overflow = Fixture::valid_three_samples();
    overflow.offsets = OffsetSpec::Absolute64(vec![u64::MAX, 0]);
    assert_eq!(
        admitted_plan(&overflow.build()),
        Err(SamplePlanError::ByteRangeOverflow)
    );

    let mut outside_source = Fixture::valid_three_samples();
    outside_source.offsets = OffsetSpec::Absolute64(vec![10_000_000, 10_000_100]);
    assert_eq!(
        admitted_plan(&outside_source.build()),
        Err(SamplePlanError::ByteRangeOutsideSource)
    );

    let mut outside_media = Fixture::valid_three_samples();
    outside_media.offsets = OffsetSpec::Absolute64(vec![8, 13]);
    assert_eq!(
        admitted_plan(&outside_media.build()),
        Err(SamplePlanError::ByteRangeOutsideMediaData)
    );

    assert!(admitted_plan(&Fixture::valid_three_samples().build()).is_ok());
}

#[test]
fn rejects_sample_and_table_run_resource_limits_then_recovers() {
    let too_many_samples = u32::try_from(MAX_PLANNED_SAMPLES_PER_TRACK + 1).unwrap();
    let sample_limit = Fixture {
        size_table: stsz_constant(1, too_many_samples),
        decode_runs: vec![(too_many_samples, 1)],
        composition_runs: None,
        chunk_runs: vec![(1, too_many_samples, 1)],
        offsets: OffsetSpec::Relative32(vec![0]),
        sync_samples: None,
        payload: vec![0],
        movie_extensions: Vec::new(),
    };
    assert_eq!(
        admitted_plan(&sample_limit.build()),
        Err(SamplePlanError::ResourceLimitExceeded(
            SamplePlanResource::Samples
        ))
    );

    let run_count = MAX_SAMPLE_TABLE_RUNS + 1;
    let table_limit = Fixture {
        size_table: stsz_constant(1, u32::try_from(run_count).unwrap()),
        decode_runs: vec![(1, 1); run_count],
        composition_runs: None,
        chunk_runs: vec![(1, u32::try_from(run_count).unwrap(), 1)],
        offsets: OffsetSpec::Relative32(vec![0]),
        sync_samples: None,
        payload: vec![0],
        movie_extensions: Vec::new(),
    };
    assert_eq!(
        admitted_plan(&table_limit.build()),
        Err(SamplePlanError::ResourceLimitExceeded(
            SamplePlanResource::TableRuns
        ))
    );

    assert!(admitted_plan(&Fixture::valid_three_samples().build()).is_ok());
}

#[test]
fn rejects_fragment_extensions_and_missing_classic_tables() {
    let mut fragmented = Fixture::valid_three_samples();
    fragmented.movie_extensions = vec![boxed(b"mvex", Vec::new())];
    let fragmented_source = fragmented.build();
    let fragmented_initialization = admit_complete_mp4(&fragmented_source).unwrap();
    assert_eq!(
        plan_non_fragmented_track_samples(
            &fragmented_source,
            &fragmented_initialization,
            fragmented_initialization.tracks[0].identity(),
        ),
        Err(SamplePlanError::FragmentedSource)
    );

    let file_type = ftyp();
    let missing_movie = movie(
        vec![track(1, b"vide", video_entry(), Vec::new())],
        Vec::new(),
    );
    let mut missing = file_type;
    missing.extend_from_slice(&missing_movie);
    missing.extend_from_slice(&boxed(b"mdat", vec![1]));
    let initialization = admit_complete_mp4(&missing).unwrap();
    assert_eq!(
        plan_non_fragmented_track_samples(
            &missing,
            &initialization,
            initialization.tracks[0].identity(),
        ),
        Err(SamplePlanError::MissingTable(SampleTable::DecodeTime))
    );
}
