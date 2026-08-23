use std::sync::{Mutex, Once};

use wild_buzzard_mp4::{
    AdmissionError, CodecFamily, EsDescriptorFailure, FourCc, MAX_CODEC_CONFIG_BYTES,
    MAX_SAMPLE_DESCRIPTIONS_PER_TRACK, MAX_SOURCE_BYTES, MAX_TRACKS, ParserFailure,
    ProtectionFailure, RequiredBox, TrackKind, UNTRUSTED_CONTENT_ADMISSION_ENABLED,
    admit_complete_mp4,
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

fn mdhd_v1(timescale: u32, duration: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u64.to_be_bytes());
    payload.extend_from_slice(&0u64.to_be_bytes());
    payload.extend_from_slice(&timescale.to_be_bytes());
    payload.extend_from_slice(&duration.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    boxed(b"mdhd", full_box(1, 0, payload))
}

fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(kind);
    payload.extend_from_slice(&[0; 12]);
    payload.push(0);
    boxed(b"hdlr", full_box(0, 0, payload))
}

fn valid_avcc() -> Vec<u8> {
    valid_avcc_with_payloads(&[0x67, 0x42, 0x00], &[0x68, 0x00])
}

fn valid_avcc_with_payloads(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    assert_eq!(sps[0] & 0x1f, 7);
    assert_eq!(pps[0] & 0x1f, 8);
    let mut config = vec![1, 66, 0, 30, 0xff, 0xe1];
    config.extend_from_slice(&u16::try_from(sps.len()).unwrap().to_be_bytes());
    config.extend_from_slice(sps);
    config.push(1);
    config.extend_from_slice(&u16::try_from(pps.len()).unwrap().to_be_bytes());
    config.extend_from_slice(pps);
    config
}

fn large_valid_avcc() -> Vec<u8> {
    const SET_BYTES: usize = 65_500;
    const SETS: usize = 16;
    let mut config = vec![1, 66, 0, 30, 0xff, 0xe0 | SETS as u8];
    for _ in 0..SETS {
        config.extend_from_slice(&(SET_BYTES as u16).to_be_bytes());
        config.push(0x67);
        config.resize(config.len() + SET_BYTES - 1, 4);
    }
    config.push(1);
    config.extend_from_slice(&1u16.to_be_bytes());
    config.push(0x68);
    assert!(config.len() <= MAX_CODEC_CONFIG_BYTES);
    config
}

fn descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 128);
    let mut result = Vec::with_capacity(payload.len() + 2);
    result.push(tag);
    result.push(payload.len() as u8);
    result.extend_from_slice(payload);
    result
}

fn four_octet_descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 0x0fff_ffff);
    let length = payload.len() as u32;
    let mut result = Vec::with_capacity(payload.len() + 5);
    result.push(tag);
    result.push(0x80 | ((length >> 21) as u8 & 0x7f));
    result.push(0x80 | ((length >> 14) as u8 & 0x7f));
    result.push(0x80 | ((length >> 7) as u8 & 0x7f));
    result.push(length as u8 & 0x7f);
    result.extend_from_slice(payload);
    result
}

fn push_bits(bits: &mut Vec<bool>, value: u32, count: usize) {
    for shift in (0..count).rev() {
        bits.push((value >> shift) & 1 != 0);
    }
}

fn audio_specific_config(object_type: u16) -> Vec<u8> {
    let mut bits = Vec::new();
    if object_type >= 32 {
        push_bits(&mut bits, 31, 5);
        push_bits(&mut bits, u32::from(object_type - 32), 6);
    } else {
        push_bits(&mut bits, u32::from(object_type), 5);
    }
    push_bits(&mut bits, 3, 4);
    push_bits(&mut bits, 2, 4);
    push_bits(&mut bits, 0, 3);
    let mut bytes = vec![0; bits.len().div_ceil(8)];
    for (index, bit) in bits.into_iter().enumerate() {
        if bit {
            bytes[index / 8] |= 1 << (7 - index % 8);
        }
    }
    bytes
}

fn esds(profile: u8, decoder_specific: Option<&[u8]>) -> Vec<u8> {
    let decoder_config = decoder_config_descriptor(profile, decoder_specific.into_iter().collect());
    esds_from_elementary_streams(vec![elementary_stream_descriptor(vec![decoder_config])])
}

fn decoder_config_descriptor(profile: u8, decoder_specific: Vec<&[u8]>) -> Vec<u8> {
    let mut decoder_config = Vec::with_capacity(32);
    decoder_config.push(profile);
    decoder_config.push(0x15);
    decoder_config.extend_from_slice(&[0; 11]);
    for config in decoder_specific {
        decoder_config.extend_from_slice(&descriptor(0x05, config));
    }
    descriptor(0x04, &decoder_config)
}

fn elementary_stream_descriptor(decoder_configs: Vec<Vec<u8>>) -> Vec<u8> {
    let mut elementary_stream = vec![0, 1, 0];
    for decoder_config in decoder_configs {
        elementary_stream.extend_from_slice(&decoder_config);
    }
    elementary_stream.extend_from_slice(&descriptor(0x06, &[2]));
    descriptor(0x03, &elementary_stream)
}

fn esds_from_elementary_streams(elementary_streams: Vec<Vec<u8>>) -> Vec<u8> {
    let mut payload = Vec::new();
    for elementary_stream in elementary_streams {
        payload.extend_from_slice(&elementary_stream);
    }
    boxed(b"esds", full_box(0, 0, payload))
}

fn four_octet_esds(profile: u8, decoder_specific: &[u8]) -> Vec<u8> {
    let mut decoder_config = vec![profile, 0x15];
    decoder_config.extend_from_slice(&[0; 11]);
    decoder_config.extend_from_slice(&four_octet_descriptor(0x05, decoder_specific));
    let decoder_config = four_octet_descriptor(0x04, &decoder_config);

    let mut elementary_stream = vec![0, 1, 0];
    elementary_stream.extend_from_slice(&decoder_config);
    elementary_stream.extend_from_slice(&four_octet_descriptor(0x06, &[2]));
    esds_from_elementary_streams(vec![four_octet_descriptor(0x03, &elementary_stream)])
}

fn video_sample_entry(kind: &[u8; 4], children: Vec<Vec<u8>>) -> Vec<u8> {
    let mut payload = vec![0; 6];
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&[0; 16]);
    payload.extend_from_slice(&640u16.to_be_bytes());
    payload.extend_from_slice(&360u16.to_be_bytes());
    payload.extend_from_slice(&[0; 50]);
    for child in children {
        payload.extend_from_slice(&child);
    }
    boxed(kind, payload)
}

fn audio_sample_entry(kind: &[u8; 4], children: Vec<Vec<u8>>) -> Vec<u8> {
    let mut payload = vec![0; 6];
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&[0; 6]);
    payload.extend_from_slice(&2u16.to_be_bytes());
    payload.extend_from_slice(&16u16.to_be_bytes());
    payload.extend_from_slice(&[0; 4]);
    payload.extend_from_slice(&(48_000u32 << 16).to_be_bytes());
    for child in children {
        payload.extend_from_slice(&child);
    }
    boxed(kind, payload)
}

fn bare_mp3_entry() -> Vec<u8> {
    audio_sample_entry(b".mp3", Vec::new())
}

fn avc_entry() -> Vec<u8> {
    video_sample_entry(b"avc1", vec![boxed(b"avcC", valid_avcc())])
}

fn track_encryption(kid: [u8; 16], constant_iv: Option<&[u8]>) -> Vec<u8> {
    let mut payload = vec![0, 0, 1, if constant_iv.is_some() { 0 } else { 16 }];
    payload.extend_from_slice(&kid);
    if let Some(iv) = constant_iv {
        payload.push(u8::try_from(iv.len()).unwrap());
        payload.extend_from_slice(iv);
    }
    boxed(b"tenc", full_box(0, 0, payload))
}

fn protection_box(original_format: &[u8; 4], kid: [u8; 16]) -> Vec<u8> {
    protection_box_with_iv(original_format, kid, None)
}

fn protection_box_with_iv(
    original_format: &[u8; 4],
    kid: [u8; 16],
    constant_iv: Option<&[u8]>,
) -> Vec<u8> {
    let frma = boxed(b"frma", original_format.to_vec());
    let mut scheme_payload = Vec::new();
    scheme_payload.extend_from_slice(b"cenc");
    scheme_payload.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    let schm = boxed(b"schm", full_box(0, 0, scheme_payload));
    let schi = boxed(b"schi", track_encryption(kid, constant_iv));
    let mut payload = Vec::new();
    payload.extend_from_slice(&frma);
    payload.extend_from_slice(&schm);
    payload.extend_from_slice(&schi);
    boxed(b"sinf", payload)
}

fn stsd_declared(declared: u32, entries: Vec<Vec<u8>>) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&declared.to_be_bytes());
    for entry in entries {
        payload.extend_from_slice(&entry);
    }
    boxed(b"stsd", full_box(0, 0, payload))
}

fn stsd(entries: Vec<Vec<u8>>) -> Vec<u8> {
    stsd_declared(u32::try_from(entries.len()).unwrap(), entries)
}

fn container(kind: &[u8; 4], children: Vec<Vec<u8>>) -> Vec<u8> {
    let mut payload = Vec::new();
    for child in children {
        payload.extend_from_slice(&child);
    }
    boxed(kind, payload)
}

fn stbl(children: Vec<Vec<u8>>) -> Vec<u8> {
    container(b"stbl", children)
}

fn minf(children: Vec<Vec<u8>>) -> Vec<u8> {
    container(b"minf", children)
}

fn mdia(children: Vec<Vec<u8>>) -> Vec<u8> {
    container(b"mdia", children)
}

fn trak(children: Vec<Vec<u8>>) -> Vec<u8> {
    container(b"trak", children)
}

fn canonical_mdia(handler: &[u8; 4], sample_description: Vec<u8>) -> Vec<u8> {
    let timescale = if handler == b"soun" { 48_000 } else { 90_000 };
    let duration = if handler == b"soun" { 240_000 } else { 450_000 };
    mdia(vec![
        mdhd(timescale, duration),
        hdlr(handler),
        minf(vec![stbl(vec![sample_description])]),
    ])
}

fn canonical_track(track_id: u32, handler: &[u8; 4], entry: Vec<u8>) -> Vec<u8> {
    let (width, height) = if handler == b"soun" {
        (0, 0)
    } else {
        (640, 360)
    };
    trak(vec![
        tkhd(track_id, width, height),
        canonical_mdia(handler, stsd(vec![entry])),
    ])
}

fn track_with_stsd(track_id: u32, handler: &[u8; 4], sample_description: Vec<u8>) -> Vec<u8> {
    let (width, height) = if handler == b"soun" {
        (0, 0)
    } else {
        (640, 360)
    };
    trak(vec![
        tkhd(track_id, width, height),
        canonical_mdia(handler, sample_description),
    ])
}

fn movie_children(children: Vec<Vec<u8>>) -> Vec<u8> {
    let mut moov = mvhd();
    for child in children {
        moov.extend_from_slice(&child);
    }
    let mut source = ftyp();
    source.extend_from_slice(&boxed(b"moov", moov));
    source
}

fn movie(tracks: Vec<Vec<u8>>) -> Vec<u8> {
    movie_children(tracks)
}

fn protected_audio_entry(profile: u8, object_type: Option<u16>, sinf_first: bool) -> Vec<u8> {
    let decoder_specific = object_type.map(audio_specific_config);
    let config = esds(profile, decoder_specific.as_deref());
    let protection = protection_box(b"mp4a", [0x51; 16]);
    let children = if sinf_first {
        vec![protection, config]
    } else {
        vec![config, protection]
    };
    audio_sample_entry(b"enca", children)
}

fn audio_entry_with_esds(config: Vec<u8>, protected: bool) -> Vec<u8> {
    if protected {
        audio_sample_entry(b"enca", vec![config, protection_box(b"mp4a", [0x5a; 16])])
    } else {
        audio_sample_entry(b"mp4a", vec![config])
    }
}

fn malformed_stts() -> Vec<u8> {
    boxed(b"stts", full_box(0, 0, 1u32.to_be_bytes().to_vec()))
}

fn pssh(system_id: [u8; 16], kid: [u8; 16], data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&system_id);
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&kid);
    payload.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    payload.extend_from_slice(data);
    boxed(b"pssh", full_box(1, 0, payload))
}

fn metadata_data(payload: &[u8]) -> Vec<u8> {
    let mut data = vec![0; 8];
    data.extend_from_slice(payload);
    boxed(b"data", data)
}

fn metadata_entry(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    boxed(kind, metadata_data(payload))
}

fn userdata_metadata(entries: Vec<Vec<u8>>) -> Vec<u8> {
    let item_list = container(b"ilst", entries);
    let metadata = boxed(b"meta", full_box(0, 0, item_list));
    container(b"udta", vec![metadata])
}

#[test]
fn admits_deterministic_video_metadata_and_exact_accounting() {
    let config = valid_avcc();
    let source = movie(vec![canonical_track(7, b"vide", avc_entry())]);
    let first = admit_complete_mp4(&source).unwrap();
    let second = admit_complete_mp4(&source).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.brands.major, FourCc(*b"isom"));
    assert_eq!(
        first.brands.compatible,
        [FourCc(*b"isom"), FourCc(*b"iso2")]
    );
    assert_eq!(first.tracks.len(), 1);
    assert_eq!(first.tracks[0].id, 7);
    assert_eq!(first.tracks[0].kind, TrackKind::Video);
    assert_eq!(first.tracks[0].duration.unwrap().nanoseconds, 5_000_000_000);
    assert_eq!(
        first.tracks[0].sample_descriptions[0].codec,
        CodecFamily::H264
    );
    assert_eq!(
        first.tracks[0].sample_descriptions[0]
            .decoder_config
            .as_deref(),
        Some(config.as_slice())
    );
    assert_eq!(first.accounting.source_bytes, source.len());
    assert_eq!(first.accounting.tracks, 1);
    assert_eq!(first.accounting.sample_descriptions, 1);
    assert_eq!(first.accounting.published_config_bytes, config.len());
    assert_eq!(first.accounting.declared_config_bytes, config.len());
    assert_eq!(first.accounting.protection_system_headers, 0);
    assert!(!first.protection_present);
    assert!(!std::hint::black_box(UNTRUSTED_CONTENT_ADMISSION_ENABLED));
}

#[test]
fn admits_audio_video_and_bare_mp3_without_provider_types() {
    let source = movie(vec![
        canonical_track(1, b"soun", bare_mp3_entry()),
        canonical_track(2, b"vide", avc_entry()),
    ]);
    let admitted = admit_complete_mp4(&source).unwrap();
    assert_eq!(admitted.tracks.len(), 2);
    assert_eq!(admitted.tracks[0].kind, TrackKind::Audio);
    assert_eq!(
        admitted.tracks[0].sample_descriptions[0].codec,
        CodecFamily::Mp3
    );
    let audio = admitted.tracks[0].sample_descriptions[0].audio.unwrap();
    assert_eq!((audio.channels, audio.sample_rate_hz), (2, 48_000));
    assert_eq!(admitted.tracks[1].kind, TrackKind::Video);
}

#[test]
fn derives_all_protected_mp4a_audio_families_in_both_child_orders() {
    let cases = [
        (0x40, Some(2), CodecFamily::Aac),
        (0x40, Some(42), CodecFamily::XheAac),
        (0x69, None, CodecFamily::Mp3),
    ];
    let mut next_id = 1;
    for (profile, object_type, expected_family) in cases {
        for sinf_first in [false, true] {
            let source = movie(vec![canonical_track(
                next_id,
                b"soun",
                protected_audio_entry(profile, object_type, sinf_first),
            )]);
            let admitted = admit_complete_mp4(&source).unwrap();
            let description = &admitted.tracks[0].sample_descriptions[0];
            assert_eq!(description.codec, expected_family);
            assert!(description.protected);
            assert!(admitted.protection_present);
            next_id += 1;
        }
    }
}

#[test]
fn separates_global_pssh_accounting_from_sample_description_protection() {
    let header = pssh([0x11; 16], [0x22; 16], &[0x33, 0x34]);
    let clear = admit_complete_mp4(&movie_children(vec![
        header.clone(),
        canonical_track(1, b"vide", avc_entry()),
    ]))
    .unwrap();
    assert_eq!(clear.accounting.protection_system_headers, 1);
    assert!(!clear.tracks[0].sample_descriptions[0].protected);
    assert!(!clear.protection_present);

    let protected_entry = video_sample_entry(
        b"encv",
        vec![
            boxed(b"avcC", valid_avcc()),
            protection_box(b"avc1", [0x44; 16]),
        ],
    );
    let protected_without_pssh = admit_complete_mp4(&movie(vec![canonical_track(
        2,
        b"vide",
        protected_entry.clone(),
    )]))
    .unwrap();
    assert_eq!(
        protected_without_pssh.accounting.protection_system_headers,
        0
    );
    assert!(protected_without_pssh.protection_present);

    let protected_with_pssh = admit_complete_mp4(&movie_children(vec![
        header,
        canonical_track(3, b"vide", protected_entry),
    ]))
    .unwrap();
    assert_eq!(protected_with_pssh.accounting.protection_system_headers, 1);
    assert!(protected_with_pssh.protection_present);
}

#[test]
fn admits_one_exact_aac_es_hierarchy_and_publishes_the_exact_dsi() {
    let decoder_specific = audio_specific_config(2);
    let entry = audio_entry_with_esds(esds(0x40, Some(&decoder_specific)), false);
    let admitted = admit_complete_mp4(&movie(vec![canonical_track(1, b"soun", entry)])).unwrap();
    let description = &admitted.tracks[0].sample_descriptions[0];
    assert_eq!(description.codec, CodecFamily::Aac);
    assert_eq!(
        description.decoder_config.as_deref(),
        Some(decoder_specific.as_slice())
    );

    let four_octet_entry = audio_entry_with_esds(four_octet_esds(0x40, &decoder_specific), false);
    let four_octet =
        admit_complete_mp4(&movie(vec![canonical_track(2, b"soun", four_octet_entry)])).unwrap();
    assert_eq!(
        four_octet.tracks[0].sample_descriptions[0]
            .decoder_config
            .as_deref(),
        Some(decoder_specific.as_slice())
    );
}

#[test]
fn rejects_ambiguous_or_malformed_es_hierarchies_for_clear_and_protected_audio_then_recovers() {
    let aac = audio_specific_config(2);
    let xhe_aac = audio_specific_config(42);
    let decoder_config = decoder_config_descriptor(0x40, vec![&aac]);
    let four_octet_decoder_config = four_octet_descriptor(0x04, &decoder_config[2..]);
    let conflicting_decoder_config = decoder_config_descriptor(0x40, vec![&xhe_aac]);
    let elementary_stream = elementary_stream_descriptor(vec![decoder_config.clone()]);
    let four_octet_elementary_stream = four_octet_descriptor(0x03, &elementary_stream[2..]);
    let conflicting_elementary_stream =
        elementary_stream_descriptor(vec![conflicting_decoder_config.clone()]);

    for protected in [false, true] {
        let duplicate_es = esds_from_elementary_streams(vec![
            elementary_stream.clone(),
            four_octet_elementary_stream.clone(),
        ]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                10,
                b"soun",
                audio_entry_with_esds(duplicate_es, protected),
            )])),
            Err(AdmissionError::EsDescriptor(
                EsDescriptorFailure::DuplicateElementaryStream
            ))
        );

        let conflicting_es = esds_from_elementary_streams(vec![
            elementary_stream.clone(),
            conflicting_elementary_stream.clone(),
        ]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                11,
                b"soun",
                audio_entry_with_esds(conflicting_es, protected),
            )])),
            Err(AdmissionError::EsDescriptor(
                EsDescriptorFailure::ConflictingElementaryStream
            ))
        );

        let duplicate_dc = esds_from_elementary_streams(vec![elementary_stream_descriptor(vec![
            decoder_config.clone(),
            four_octet_decoder_config.clone(),
        ])]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                12,
                b"soun",
                audio_entry_with_esds(duplicate_dc, protected),
            )])),
            Err(AdmissionError::EsDescriptor(
                EsDescriptorFailure::DuplicateDecoderConfig
            ))
        );

        let conflicting_dc =
            esds_from_elementary_streams(vec![elementary_stream_descriptor(vec![
                decoder_config.clone(),
                conflicting_decoder_config.clone(),
            ])]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                13,
                b"soun",
                audio_entry_with_esds(conflicting_dc, protected),
            )])),
            Err(AdmissionError::EsDescriptor(
                EsDescriptorFailure::ConflictingDecoderConfig
            ))
        );

        let duplicate_dsi = esds_from_elementary_streams(vec![elementary_stream_descriptor(vec![
            decoder_config_descriptor(0x40, vec![&aac, &aac]),
        ])]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                14,
                b"soun",
                audio_entry_with_esds(duplicate_dsi, protected),
            )])),
            Err(AdmissionError::EsDescriptor(
                EsDescriptorFailure::DuplicateDecoderSpecificInfo
            ))
        );

        let conflicting_dsi =
            esds_from_elementary_streams(vec![elementary_stream_descriptor(vec![
                decoder_config_descriptor(0x40, vec![&aac, &xhe_aac]),
            ])]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                15,
                b"soun",
                audio_entry_with_esds(conflicting_dsi, protected),
            )])),
            Err(AdmissionError::EsDescriptor(
                EsDescriptorFailure::ConflictingDecoderSpecificInfo
            ))
        );

        let mut truncated_elementary_stream = elementary_stream.clone();
        truncated_elementary_stream.pop();
        let malformed = esds_from_elementary_streams(vec![truncated_elementary_stream]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                16,
                b"soun",
                audio_entry_with_esds(malformed, protected),
            )])),
            Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))
        );

        let mut trailing_byte = elementary_stream.clone();
        trailing_byte.push(0xa5);
        let inexact_end = esds_from_elementary_streams(vec![trailing_byte]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                18,
                b"soun",
                audio_entry_with_esds(inexact_end, protected),
            )])),
            Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))
        );

        let mut invalid_stream_type = decoder_config.clone();
        invalid_stream_type[3] = 0;
        let invalid_stream_type =
            esds_from_elementary_streams(vec![elementary_stream_descriptor(vec![
                invalid_stream_type,
            ])]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                19,
                b"soun",
                audio_entry_with_esds(invalid_stream_type, protected),
            )])),
            Err(AdmissionError::EsDescriptor(EsDescriptorFailure::Malformed))
        );

        let recovered = audio_entry_with_esds(esds(0x40, Some(&aac)), protected);
        assert!(admit_complete_mp4(&movie(vec![canonical_track(17, b"soun", recovered)])).is_ok());
    }
}

#[test]
fn rejects_missing_es_decoder_config_and_aac_dsi_records() {
    let missing_es = boxed(b"esds", full_box(0, 0, Vec::new()));
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(
            1,
            b"soun",
            audio_entry_with_esds(missing_es, false),
        )])),
        Err(AdmissionError::EsDescriptor(
            EsDescriptorFailure::MissingElementaryStream
        ))
    );

    let missing_decoder_config = esds_from_elementary_streams(vec![descriptor(0x03, &[0, 1, 0])]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(
            2,
            b"soun",
            audio_entry_with_esds(missing_decoder_config, false),
        )])),
        Err(AdmissionError::EsDescriptor(
            EsDescriptorFailure::MissingDecoderConfig
        ))
    );

    let missing_decoder_specific = esds(0x40, None);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(
            3,
            b"soun",
            audio_entry_with_esds(missing_decoder_specific, true),
        )])),
        Err(AdmissionError::EsDescriptor(
            EsDescriptorFailure::MissingDecoderSpecificInfo
        ))
    );
}

#[test]
fn rejects_malformed_audio_configs_and_audio_original_format_mismatch_then_recovers() {
    let truncated_aac = audio_sample_entry(b"mp4a", vec![esds(0x40, Some(&[0x11]))]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(1, b"soun", truncated_aac,)])),
        Err(AdmissionError::EsDescriptor(
            EsDescriptorFailure::ParserDecoderSpecificMismatch
        ))
    );

    let wrong_rate = audio_sample_entry(b"mp4a", vec![esds(0x40, Some(&[0x12, 0x10]))]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(2, b"soun", wrong_rate)])),
        Err(AdmissionError::MalformedCodecConfiguration)
    );

    let aac = esds(0x40, Some(&audio_specific_config(2)));
    let mismatch = audio_sample_entry(b"enca", vec![aac, protection_box(b".mp3", [0x52; 16])]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(3, b"soun", mismatch)])),
        Err(AdmissionError::Protection(ProtectionFailure::CodecMismatch))
    );

    assert!(
        admit_complete_mp4(&movie(vec![canonical_track(
            4,
            b"soun",
            protected_audio_entry(0x40, Some(2), false),
        )]))
        .is_ok()
    );
}

#[test]
fn rejects_missing_duplicate_and_conflicting_protection_then_recovers() {
    let config = boxed(b"avcC", valid_avcc());
    let valid = protection_box(b"avc1", [7; 16]);
    let conflicting = protection_box(b"hvc1", [8; 16]);
    let missing = video_sample_entry(b"encv", vec![config.clone()]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(1, b"vide", missing)])),
        Err(AdmissionError::Protection(ProtectionFailure::Missing))
    );

    let duplicate = video_sample_entry(b"encv", vec![config.clone(), valid.clone(), valid.clone()]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(2, b"vide", duplicate)])),
        Err(AdmissionError::Protection(ProtectionFailure::Duplicate))
    );

    let conflict = video_sample_entry(b"encv", vec![config.clone(), valid.clone(), conflicting]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(3, b"vide", conflict)])),
        Err(AdmissionError::Protection(ProtectionFailure::Conflicting))
    );

    let recovered = video_sample_entry(b"encv", vec![valid, config]);
    assert!(admit_complete_mp4(&movie(vec![canonical_track(4, b"vide", recovered)])).is_ok());
}

#[test]
fn rejects_incomplete_protection_singletons() {
    let config = boxed(b"avcC", valid_avcc());
    let incomplete = boxed(b"sinf", boxed(b"frma", b"avc1".to_vec()));
    let entry = video_sample_entry(b"encv", vec![config, incomplete]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(1, b"vide", entry)])),
        Err(AdmissionError::Protection(ProtectionFailure::Incomplete))
    );

    let mut duplicate_frma = boxed(b"frma", b"avc1".to_vec());
    duplicate_frma.extend_from_slice(&boxed(b"frma", b"avc1".to_vec()));
    let duplicate = video_sample_entry(
        b"encv",
        vec![boxed(b"avcC", valid_avcc()), boxed(b"sinf", duplicate_frma)],
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(2, b"vide", duplicate)])),
        Err(AdmissionError::Protection(ProtectionFailure::Duplicate))
    );
}

#[test]
fn rejects_avc_hevc_original_format_mismatches_before_disabled_family_policy() {
    let avc_as_hevc = video_sample_entry(
        b"encv",
        vec![
            boxed(b"avcC", valid_avcc()),
            protection_box(b"hvc1", [1; 16]),
        ],
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(1, b"vide", avc_as_hevc)])),
        Err(AdmissionError::Protection(ProtectionFailure::CodecMismatch))
    );

    let hevc_as_avc = video_sample_entry(
        b"encv",
        vec![
            boxed(b"hvcC", vec![1, 2, 3, 4]),
            protection_box(b"avc1", [2; 16]),
        ],
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(2, b"vide", hevc_as_avc)])),
        Err(AdmissionError::Protection(ProtectionFailure::CodecMismatch))
    );

    let matching_but_disabled = video_sample_entry(
        b"encv",
        vec![
            protection_box(b"hvc1", [3; 16]),
            boxed(b"hvcC", vec![1, 2, 3, 4]),
        ],
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(
            3,
            b"vide",
            matching_but_disabled,
        )])),
        Err(AdmissionError::UnsupportedCodecConfiguration)
    );
}

#[test]
fn enforces_exact_stsd_declared_framed_and_parsed_cardinality_then_recovers() {
    let over_declared = stsd_declared(2, vec![avc_entry()]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![track_with_stsd(1, b"vide", over_declared,)])),
        Err(AdmissionError::SampleDescriptionCountMismatch)
    );

    let hidden_extra = stsd_declared(1, vec![avc_entry(), avc_entry()]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![track_with_stsd(2, b"vide", hidden_extra,)])),
        Err(AdmissionError::SampleDescriptionCountMismatch)
    );

    let mut truncated_entry = avc_entry();
    truncated_entry.pop();
    let truncated = stsd_declared(1, vec![truncated_entry]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![track_with_stsd(3, b"vide", truncated)])),
        Err(AdmissionError::MalformedStructure)
    );

    assert_eq!(
        admit_complete_mp4(&movie(vec![track_with_stsd(
            4,
            b"vide",
            stsd_declared(0, Vec::new()),
        )])),
        Err(AdmissionError::MissingSampleDescription)
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![track_with_stsd(
            5,
            b"vide",
            stsd_declared(
                u32::try_from(MAX_SAMPLE_DESCRIPTIONS_PER_TRACK + 1).unwrap(),
                Vec::new(),
            ),
        )])),
        Err(AdmissionError::TooManySampleDescriptions)
    );
    assert!(admit_complete_mp4(&movie(vec![canonical_track(6, b"vide", avc_entry())])).is_ok());
}

#[test]
fn validates_singleton_track_hierarchy_and_rejects_cross_mdia_merging() {
    let duplicate_tkhd = trak(vec![
        tkhd(1, 640, 360),
        tkhd(1, 640, 360),
        canonical_mdia(b"vide", stsd(vec![avc_entry()])),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![duplicate_tkhd])),
        Err(AdmissionError::DuplicateRequiredBox(
            RequiredBox::TrackHeader
        ))
    );

    let duplicate_mdhd = trak(vec![
        tkhd(2, 640, 360),
        mdia(vec![
            mdhd(90_000, 450_000),
            mdhd(90_000, 450_000),
            hdlr(b"vide"),
            minf(vec![stbl(vec![stsd(vec![avc_entry()])])]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![duplicate_mdhd])),
        Err(AdmissionError::DuplicateRequiredBox(
            RequiredBox::MediaHeader
        ))
    );

    let duplicate_handler = trak(vec![
        tkhd(22, 640, 360),
        mdia(vec![
            mdhd(90_000, 450_000),
            hdlr(b"vide"),
            hdlr(b"vide"),
            minf(vec![stbl(vec![stsd(vec![avc_entry()])])]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![duplicate_handler])),
        Err(AdmissionError::DuplicateRequiredBox(RequiredBox::Handler))
    );

    let duplicate_stsd = trak(vec![
        tkhd(3, 640, 360),
        mdia(vec![
            mdhd(90_000, 450_000),
            hdlr(b"vide"),
            minf(vec![stbl(vec![
                stsd(vec![avc_entry()]),
                stsd(vec![avc_entry()]),
            ])]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![duplicate_stsd])),
        Err(AdmissionError::DuplicateRequiredBox(
            RequiredBox::SampleDescription
        ))
    );

    let duplicate_media = trak(vec![
        tkhd(23, 640, 360),
        canonical_mdia(b"vide", stsd(vec![avc_entry()])),
        canonical_mdia(b"vide", stsd(vec![avc_entry()])),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![duplicate_media])),
        Err(AdmissionError::DuplicateRequiredBox(
            RequiredBox::TrackMedia
        ))
    );

    let cross_mdia = trak(vec![
        tkhd(4, 640, 360),
        mdia(vec![mdhd(90_000, 450_000)]),
        mdia(vec![
            hdlr(b"vide"),
            minf(vec![stbl(vec![stsd(vec![avc_entry()])])]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![cross_mdia])),
        Err(AdmissionError::DuplicateRequiredBox(
            RequiredBox::TrackMedia
        ))
    );

    assert!(admit_complete_mp4(&movie(vec![canonical_track(5, b"vide", avc_entry())])).is_ok());
}

#[test]
fn rejects_parser_sensitive_box_reordering_and_multiple_movies() {
    let reordered_media = trak(vec![
        tkhd(1, 640, 360),
        mdia(vec![
            hdlr(b"vide"),
            mdhd(90_000, 450_000),
            minf(vec![stbl(vec![stsd(vec![avc_entry()])])]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![reordered_media])),
        Err(AdmissionError::InvalidBoxOrder)
    );

    let media_before_header = trak(vec![
        canonical_mdia(b"vide", stsd(vec![avc_entry()])),
        tkhd(2, 640, 360),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![media_before_header])),
        Err(AdmissionError::InvalidBoxOrder)
    );

    let mut multiple = movie(vec![canonical_track(3, b"vide", avc_entry())]);
    let mut second_movie = mvhd();
    second_movie.extend_from_slice(&canonical_track(4, b"vide", avc_entry()));
    multiple.extend_from_slice(&boxed(b"moov", second_movie));
    assert_eq!(
        admit_complete_mp4(&multiple),
        Err(AdmissionError::DuplicateMovie)
    );
}

#[test]
fn validates_avcc_structure_and_rejects_unvalidated_mp4v() {
    let mut wrong_nal_type = valid_avcc();
    wrong_nal_type[8] = 0x68;
    let malformed = [
        vec![1, 66, 0, 30, 0xff, 0xe0, 0],
        vec![1, 66, 0, 30, 0xff, 0xe1, 0, 4, 0x67],
        wrong_nal_type,
    ];
    for (index, config) in malformed.into_iter().enumerate() {
        let entry = video_sample_entry(b"avc1", vec![boxed(b"avcC", config)]);
        assert_eq!(
            admit_complete_mp4(&movie(vec![canonical_track(
                u32::try_from(index + 1).unwrap(),
                b"vide",
                entry,
            )])),
            Err(AdmissionError::MalformedCodecConfiguration)
        );
    }

    let wrong_entry = video_sample_entry(b"hvc1", vec![boxed(b"avcC", valid_avcc())]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(8, b"vide", wrong_entry,)])),
        Err(AdmissionError::SampleDescriptionKindMismatch)
    );

    let mp4v = video_sample_entry(b"mp4v", vec![boxed(b"esds", full_box(0, 0, vec![1, 2, 3]))]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(9, b"vide", mp4v)])),
        Err(AdmissionError::UnsupportedCodecConfiguration)
    );
    assert!(admit_complete_mp4(&movie(vec![canonical_track(10, b"vide", avc_entry())])).is_ok());
}

#[test]
fn rejects_configuration_quantity_per_box_and_aggregate_limits() {
    let missing = video_sample_entry(b"avc1", Vec::new());
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(1, b"vide", missing)])),
        Err(AdmissionError::InvalidCodecConfigurationQuantity)
    );

    let duplicate = video_sample_entry(
        b"avc1",
        vec![boxed(b"avcC", valid_avcc()), boxed(b"avcC", valid_avcc())],
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(2, b"vide", duplicate)])),
        Err(AdmissionError::InvalidCodecConfigurationQuantity)
    );

    let too_large = video_sample_entry(
        b"avc1",
        vec![boxed(b"avcC", vec![3; MAX_CODEC_CONFIG_BYTES + 1])],
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(3, b"vide", too_large)])),
        Err(AdmissionError::CodecConfigurationTooLarge)
    );

    let config = large_valid_avcc();
    let tracks = (10..15)
        .map(|id| {
            canonical_track(
                id,
                b"vide",
                video_sample_entry(b"avc1", vec![boxed(b"avcC", config.clone())]),
            )
        })
        .collect();
    assert_eq!(
        admit_complete_mp4(&movie(tracks)),
        Err(AdmissionError::DeclaredConfigurationBudgetExceeded)
    );
}

#[test]
fn rejects_source_track_identity_and_time_policies_then_recovers() {
    assert_eq!(
        admit_complete_mp4(&vec![0; MAX_SOURCE_BYTES + 1]),
        Err(AdmissionError::SourceTooLarge)
    );
    let tracks = (1..=u32::try_from(MAX_TRACKS + 1).unwrap())
        .map(|id| canonical_track(id, b"vide", avc_entry()))
        .collect();
    assert_eq!(
        admit_complete_mp4(&movie(tracks)),
        Err(AdmissionError::TooManyTracks)
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![
            canonical_track(9, b"soun", bare_mp3_entry()),
            canonical_track(9, b"vide", avc_entry()),
        ])),
        Err(AdmissionError::DuplicateTrackIdentity)
    );
    assert_eq!(
        admit_complete_mp4(&movie(vec![canonical_track(0, b"vide", avc_entry())])),
        Err(AdmissionError::InvalidTrackIdentity)
    );

    let overflow_track = trak(vec![
        tkhd(20, 640, 360),
        mdia(vec![
            mdhd_v1(1, u64::MAX - 1),
            hdlr(b"vide"),
            minf(vec![stbl(vec![stsd(vec![avc_entry()])])]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![overflow_track])),
        Err(AdmissionError::TimeOverflow)
    );
    assert!(admit_complete_mp4(&movie(vec![canonical_track(21, b"vide", avc_entry())])).is_ok());
}

#[test]
fn maps_parser_failure_without_diagnostic_or_partial_publication() {
    let sample_table = stbl(vec![stsd(vec![avc_entry()]), malformed_stts()]);
    let track = trak(vec![
        tkhd(1, 640, 360),
        mdia(vec![
            mdhd(90_000, 450_000),
            hdlr(b"vide"),
            minf(vec![sample_table]),
        ]),
    ]);
    assert_eq!(
        admit_complete_mp4(&movie(vec![track])),
        Err(AdmissionError::Parser(ParserFailure::Truncated))
    );
    assert!(admit_complete_mp4(&movie(vec![canonical_track(2, b"vide", avc_entry())])).is_ok());
}

#[test]
fn rejects_truncated_box_and_movie_without_tracks() {
    let source = movie(vec![canonical_track(1, b"vide", avc_entry())]);
    assert_eq!(
        admit_complete_mp4(&source[..source.len() - 1]),
        Err(AdmissionError::MalformedStructure)
    );
    let mut no_tracks = ftyp();
    no_tracks.extend_from_slice(&boxed(b"moov", mvhd()));
    assert_eq!(
        admit_complete_mp4(&no_tracks),
        Err(AdmissionError::NoTracks)
    );
}

struct CapturingLogger {
    records: Mutex<Vec<String>>,
}

impl log::Log for CapturingLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        self.records.lock().unwrap().push(record.args().to_string());
    }

    fn flush(&self) {}
}

static LOGGER: CapturingLogger = CapturingLogger {
    records: Mutex::new(Vec::new()),
};
static INSTALL_LOGGER: Once = Once::new();

fn captured_logs<T>(operation: impl FnOnce() -> T) -> (T, String) {
    INSTALL_LOGGER.call_once(|| {
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
    LOGGER.records.lock().unwrap().clear();
    let result = operation();
    let logs = LOGGER.records.lock().unwrap().join("\n");
    (result, logs)
}

fn decimal_sequence(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn parser_logging_redacts_all_media_payloads_on_success_and_later_failure() {
    let config_sentinel = [0x67, 201, 202, 203];
    let pps = [0x68, 204, 205];
    let kid = [
        211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226,
    ];
    let iv = [231, 232, 233, 234, 235, 236, 237, 238];
    let pssh_system = [
        151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166,
    ];
    let pssh_kid = [
        171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186,
    ];
    let pssh_data = [191, 192, 193, 194, 195, 196, 197, 198];
    let title_sentinel: &[u8] = b"WB_C3_TITLE_SENTINEL";
    let text_sentinel: &[u8] = b"WB_C3_TEXT_SENTINEL";
    let url_sentinel: &[u8] = b"https://c3.invalid/WB_C3_URL_SENTINEL";
    let owner_sentinel: &[u8] = b"WB_C3_OWNER_SENTINEL";
    let cover_art_sentinel: &[u8] = b"WB_C3_COVER_ART_SENTINEL";
    let protected_entry = video_sample_entry(
        b"encv",
        vec![
            boxed(b"avcC", valid_avcc_with_payloads(&config_sentinel, &pps)),
            protection_box_with_iv(b"avc1", kid, Some(&iv)),
        ],
    );
    let header = pssh(pssh_system, pssh_kid, &pssh_data);
    let userdata = userdata_metadata(vec![
        metadata_entry(&[0xa9, b'n', b'a', b'm'], title_sentinel),
        metadata_entry(b"desc", text_sentinel),
        metadata_entry(b"purl", url_sentinel),
        metadata_entry(b"ownr", owner_sentinel),
        metadata_entry(b"covr", cover_art_sentinel),
    ]);
    let success_source = movie_children(vec![
        header.clone(),
        userdata.clone(),
        canonical_track(1, b"vide", protected_entry.clone()),
    ]);
    let (success, success_logs) = captured_logs(|| {
        let admitted = admit_complete_mp4(&success_source);

        let mut parser_source = success_source.as_slice();
        let parsed = mp4parse::read_mp4(&mut parser_source, mp4parse::ParseStrictness::Normal)
            .expect("the successful admission fixture must remain parser-valid");
        let metadata = parsed
            .userdata
            .expect("the fixture must contain userdata")
            .expect("the fixture userdata must parse")
            .meta
            .expect("the fixture must contain metadata");
        assert_eq!(
            metadata.title.as_ref().map(|value| value.as_slice()),
            Some(title_sentinel)
        );
        assert_eq!(
            metadata.description.as_ref().map(|value| value.as_slice()),
            Some(text_sentinel)
        );
        assert_eq!(
            metadata.podcast_url.as_ref().map(|value| value.as_slice()),
            Some(url_sentinel)
        );
        assert_eq!(
            metadata.owner.as_ref().map(|value| value.as_slice()),
            Some(owner_sentinel)
        );
        let cover_art = metadata
            .cover_art
            .expect("the fixture must contain cover art");
        assert_eq!(cover_art.len(), 1);
        assert_eq!(cover_art[0].as_slice(), cover_art_sentinel);

        admitted
    });
    assert!(success.is_ok());

    let failed_track = trak(vec![
        tkhd(2, 640, 360),
        mdia(vec![
            mdhd(90_000, 450_000),
            hdlr(b"vide"),
            minf(vec![stbl(vec![
                stsd(vec![protected_entry]),
                malformed_stts(),
            ])]),
        ]),
    ]);
    let failure_source = movie_children(vec![header, userdata, failed_track]);
    let (failure, failure_logs) = captured_logs(|| admit_complete_mp4(&failure_source));
    assert_eq!(
        failure,
        Err(AdmissionError::Parser(ParserFailure::Truncated))
    );

    for sentinel in [
        config_sentinel.as_slice(),
        pps.as_slice(),
        kid.as_slice(),
        iv.as_slice(),
        pssh_system.as_slice(),
        pssh_kid.as_slice(),
        pssh_data.as_slice(),
        title_sentinel,
        text_sentinel,
        url_sentinel,
        owner_sentinel,
        cover_art_sentinel,
    ] {
        let sequence = decimal_sequence(sentinel);
        assert!(!success_logs.contains(&sequence));
        assert!(!failure_logs.contains(&sequence));

        let printable = String::from_utf8_lossy(sentinel);
        assert!(!success_logs.contains(printable.as_ref()));
        assert!(!failure_logs.contains(printable.as_ref()));
    }

    for logs in [&success_logs, &failure_logs] {
        assert!(logs.contains("parsed userdata: metadata_present=true, cover_art_entries=1"));
        assert!(logs.contains("parsed protection-system header: kid_count=1"));
        assert!(logs.contains("parsed 1 sample descriptions"));
    }
}
