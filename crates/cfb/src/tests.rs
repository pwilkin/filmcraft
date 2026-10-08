use crate::*;

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u32).wrapping_mul(31).wrapping_add(seed as u32) as u8).collect()
}

/// A tree with mini and regular streams, nested storages and many siblings.
fn sample(version: Version, siblings: usize) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let mut w = Writer::new(version);
    w.set_clsid(Writer::ROOT, [7; 16]);
    let mut expect = Vec::new();
    let a = w.storage(Writer::ROOT, "Header-2", [1; 16]).unwrap();
    let b = w.storage(a, "Content-3b03", [2; 16]).unwrap();
    for (i, size) in [0usize, 1, 63, 64, 65, 4095, 4096, 4097, 20_000].iter().enumerate() {
        let name = format!("s{i}");
        let data = pattern(*size, i as u8);
        w.stream(b, &name, data.clone()).unwrap();
        expect.push((format!("Header-2/Content-3b03/{name}"), data));
    }
    for i in 0..siblings {
        let name = format!("Mobs-1901{{{i:x}}}");
        let s = w.storage(Writer::ROOT, &name, [3; 16]).unwrap();
        let data = pattern(10 + i * 7, 99);
        w.stream(s, "properties", data.clone()).unwrap();
        expect.push((format!("{name}/properties"), data));
    }
    (w.finish(), expect)
}

fn check(bytes: &[u8], expect: &[(String, Vec<u8>)]) {
    let cf = CompoundFile::open(bytes).unwrap();
    assert_eq!(cf.root().clsid, [7; 16]);
    for (path, data) in expect {
        assert_eq!(&cf.read_path(path).unwrap(), data, "{path}");
    }
    let h = cf.find("Header-2").unwrap();
    assert_eq!(cf.entries[h].clsid, [1; 16]);
    assert_eq!(cf.entries[h].kind, EntryKind::Storage);
    // children come back sorted in CFB order
    for e in &cf.entries {
        for w in e.children.windows(2) {
            assert!(compare_names(&cf.entries[w[0]].name, &cf.entries[w[1]].name).is_lt());
        }
    }
}

/// Every path from a node to a nil leaf has the same number of black nodes, and no red node has
/// a red child.
fn check_red_black(bytes: &[u8]) {
    let cf = CompoundFile::open(bytes).unwrap();
    let raw = |i: u32| -> (u32, u32, u8) {
        let ss = cf.sector_size();
        let dir_start = u32::from_le_bytes(bytes[48..52].try_into().unwrap());
        // the writer stores the directory contiguously
        let at = (dir_start as usize + 1) * ss + i as usize * 128;
        let e = &bytes[at..at + 128];
        (u32::from_le_bytes(e[68..72].try_into().unwrap()), u32::from_le_bytes(e[72..76].try_into().unwrap()), e[67])
    };
    fn black_height(raw: &dyn Fn(u32) -> (u32, u32, u8), n: u32, parent_red: bool) -> usize {
        if n == NOSTREAM {
            return 1;
        }
        let (l, r, c) = raw(n);
        let red = c == 0;
        assert!(!(red && parent_red), "red node with a red parent");
        let hl = black_height(raw, l, red);
        let hr = black_height(raw, r, red);
        assert_eq!(hl, hr, "unbalanced black height");
        hl + usize::from(!red)
    }
    let dir_start = u32::from_le_bytes(bytes[48..52].try_into().unwrap());
    let ss = cf.sector_size();
    for i in 0..cf.entries.len() {
        let at = (dir_start as usize + 1) * ss + i * 128;
        let child = u32::from_le_bytes(bytes[at + 76..at + 80].try_into().unwrap());
        if child != NOSTREAM {
            let (_, _, c) = raw(child);
            assert_eq!(c, 1, "tree roots are black");
            black_height(&raw, child, false);
        }
    }
}

#[test]
fn round_trip_v3_and_v4() {
    for v in [Version::V3, Version::V4] {
        for n in [0, 1, 2, 3, 7, 8, 40] {
            let (bytes, expect) = sample(v, n);
            assert_eq!(bytes.len() % v_sector(v), 0);
            check(&bytes, &expect);
            check_red_black(&bytes);
        }
    }
}

fn v_sector(v: Version) -> usize {
    if v == Version::V3 { 512 } else { 4096 }
}

#[test]
fn header_fields() {
    let (b, _) = sample(Version::V4, 3);
    assert!(sniff(&b));
    assert_eq!(&b[24..26], &0x3Eu16.to_le_bytes());
    assert_eq!(&b[26..28], &4u16.to_le_bytes());
    assert_eq!(&b[30..32], &12u16.to_le_bytes());
    let (b, _) = sample(Version::V3, 3);
    assert_eq!(&b[26..28], &3u16.to_le_bytes());
    assert_eq!(&b[30..32], &9u16.to_le_bytes());
    assert_eq!(&b[40..44], &0u32.to_le_bytes(), "version 3 leaves the directory sector count 0");
    assert_eq!(CompoundFile::open(&b).unwrap().version, 3);
}

#[test]
fn large_file_needs_difat() {
    // > 109 FAT sectors with 512-byte sectors: 109 * 128 sectors * 512 bytes ≈ 7.1 MB
    let mut w = Writer::new(Version::V3);
    let big = pattern(7_500_000, 5);
    w.stream(Writer::ROOT, "big", big.clone()).unwrap();
    w.stream(Writer::ROOT, "small", vec![1, 2, 3]).unwrap();
    let b = w.finish();
    let n_difat = u32::from_le_bytes(b[72..76].try_into().unwrap());
    assert!(n_difat >= 1);
    let cf = CompoundFile::open(&b).unwrap();
    assert_eq!(cf.read_path("big").unwrap(), big);
    assert_eq!(cf.read_path("small").unwrap(), vec![1, 2, 3]);
}

#[test]
fn names() {
    assert!(compare_names("b", "aa").is_lt(), "shorter first");
    assert!(compare_names("abc", "ABD").is_lt());
    assert!(compare_names("Abc", "aBC").is_eq());
    assert!(validate_name("a/b").is_err());
    assert!(validate_name(&"x".repeat(32)).is_err());
    assert!(validate_name(&"x".repeat(31)).is_ok());
    let mut w = Writer::default();
    w.stream(Writer::ROOT, "Name", vec![]).unwrap();
    assert!(matches!(w.stream(Writer::ROOT, "NAME", vec![]), Err(Error::BadName(_))));
    let s = w.stream(Writer::ROOT, "x", vec![]).unwrap();
    assert!(w.storage(s, "y", [0; 16]).is_err(), "streams have no children");
    let cf_bytes = w.finish();
    let cf = CompoundFile::open(&cf_bytes).unwrap();
    assert!(cf.find("name").is_ok(), "lookups are case-insensitive");
    assert!(matches!(cf.find("nope"), Err(Error::NotFound(_))));
    assert!(cf.read(0).is_err(), "the root is not a stream");
}

#[test]
fn unicode_names() {
    let mut w = Writer::default();
    w.stream(Writer::ROOT, "Ünïcødé ストリーム", vec![9; 10]).unwrap();
    let b = w.finish();
    let cf = CompoundFile::open(&b).unwrap();
    assert_eq!(cf.read_path("üNÏCØDÉ ストリーム").unwrap(), vec![9; 10]);
}

#[test]
fn not_cfb() {
    assert_eq!(CompoundFile::open(b"hello").unwrap_err(), Error::NotCfb);
    assert_eq!(CompoundFile::open(&[0u8; 600]).unwrap_err(), Error::NotCfb);
}

#[test]
fn truncation_never_panics() {
    let (b, expect) = sample(Version::V3, 20);
    let mut ok = 0;
    for cut in (0..b.len()).step_by(29) {
        if let Ok(cf) = CompoundFile::open(&b[..cut]) {
            ok += 1;
            for (path, data) in &expect {
                if let Ok(d) = cf.read_path(path) {
                    assert_eq!(&d, data, "a stream that reads back is complete");
                }
            }
        }
    }
    let _ = ok;
}

#[test]
fn mutation_never_panics_or_hangs() {
    let (b, _) = sample(Version::V3, 12);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..1500 {
        let mut g = b.clone();
        for _ in 0..6 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            // bias towards the header, FAT and directory (the end of the file)
            let at = if seed & 1 == 0 { (seed >> 8) as usize % 512 } else { g.len() - 1 - (seed >> 8) as usize % 2048.min(g.len()) };
            g[at] = (seed >> 40) as u8;
        }
        if let Ok(cf) = CompoundFile::open(&g) {
            for i in 0..cf.entries.len() {
                let _ = cf.read(i);
            }
        }
    }
}

/// A compound file of `sectors` sectors (after the header) with sector shift `shift`, whose header
/// names sector 0 as every FAT sector; sector 0 is zeros, so the FAT maps every sector to 0 and
/// the directory chain (starting at 0) loops on itself.
fn looping_file(shift: u16, sectors: usize) -> Vec<u8> {
    let ss = 1usize << shift;
    let mut b = vec![0u8; (ss * (sectors + 1)).max(1024)];
    let n_fat = sectors.min(109) as u32;
    b[..8].copy_from_slice(&SIGNATURE);
    b[24..26].copy_from_slice(&0x3Eu16.to_le_bytes());
    b[26..28].copy_from_slice(&(if shift == 12 { 4u16 } else { 3 }).to_le_bytes());
    b[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
    b[30..32].copy_from_slice(&shift.to_le_bytes());
    b[32..34].copy_from_slice(&6u16.to_le_bytes());
    b[44..48].copy_from_slice(&n_fat.to_le_bytes());
    b[48..52].copy_from_slice(&0u32.to_le_bytes());
    b[56..60].copy_from_slice(&MINI_STREAM_CUTOFF.to_le_bytes());
    b[60..64].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    b[68..72].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    for i in 0..109u32 {
        let v = if i < n_fat { 0 } else { FREESECT };
        let at = 76 + i as usize * 4;
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// Sector shift 16 (64 KiB sectors) was accepted, and a looping chain appended a whole sector per
/// FAT entry before the loop was noticed (~1 GiB from a 128 KiB file). Only the spec's shifts (9
/// and 12) are accepted, and a chain stops after as many steps as the file has sectors.
#[test]
fn looping_chains_do_not_amplify() {
    for shift in [7u16, 8, 10, 11, 13, 16] {
        let e = CompoundFile::open(&looping_file(shift, 1)).unwrap_err();
        assert!(matches!(&e, Error::Invalid(m) if m.contains("sector shift")), "{shift}: {e:?}");
    }
    // 109 FAT sectors that are all sector 0: the FAT has 109 × 1024 entries, the file 109 sectors
    // (before: ~450 MiB appended before the loop was noticed)
    let started = std::time::Instant::now();
    for shift in [9u16, 12] {
        let e = CompoundFile::open(&looping_file(shift, 109)).unwrap_err();
        assert!(matches!(&e, Error::Invalid(m) if m.contains("loops")), "{shift}: {e:?}");
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
}
