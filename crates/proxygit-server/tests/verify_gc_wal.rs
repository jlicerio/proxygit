//! Ad-hoc verification: CRC32C WAL + Block GC.
//!
//! Exercises the new public API surface that unit tests don't fully reach.
//! Defined as a single `#[test]` function so cargo's test harness runs it.

#[test]
fn verify_crc32c_wal_and_block_gc() {
    let root = std::path::PathBuf::from(std::env::temp_dir()).join("hermes-verify-run");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    eprintln!("=== Ad-hoc verification: CRC32C WAL + Block GC ===");

    // ── 1. CRC32C static write/read round-trip ──
    eprintln!("[1/4] CRC32C write_staged_records + read_staged_records round-trip...");
    let stage = root.join("stage_crc.wal");
    let rec1 = proxygit_client::wal::WalRecord {
        seq: 1,
        path: "a.rs".into(),
        offset: 0,
        data: b"hello".to_vec(),
    };
    proxygit_client::wal::LocalWal::write_staged_records(&stage, &[rec1]).unwrap();
    let got = proxygit_client::wal::LocalWal::read_staged_records(&stage).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].seq, 1);
    assert_eq!(got[0].path, "a.rs");
    assert_eq!(got[0].data, b"hello");
    eprintln!("   PASS: clean round-trip\n");

    // ── 2. CRC rejects corruption silently ──
    eprintln!("[2/4] CRC detection on corrupted data...");
    let mut raw = std::fs::read(&stage).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0xAA;
    std::fs::write(&stage, &raw).unwrap();
    let got2 = proxygit_client::wal::LocalWal::read_staged_records(&stage).unwrap();
    assert_eq!(
        got2.len(),
        0,
        "CRC mismatch must silently drop corrupt record"
    );
    eprintln!("   PASS: corrupt record silently dropped\n");

    // ── 3. Truncated tail silently ignored ──
    eprintln!("[3/4] Truncated tail handling...");
    let stage2 = root.join("stage_trunc.wal");
    let rec2 = proxygit_client::wal::WalRecord {
        seq: 2,
        path: "b.rs".into(),
        offset: 10,
        data: b"world".to_vec(),
    };
    proxygit_client::wal::LocalWal::write_staged_records(&stage2, &[rec2]).unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&stage2)
            .unwrap();
        f.write_all(&[0x50, 0x47]).unwrap(); // partial "PG" magic
        f.flush().unwrap();
    }
    let got3 = proxygit_client::wal::LocalWal::read_staged_records(&stage2).unwrap();
    assert_eq!(
        got3.len(),
        1,
        "truncated tail must silently drop, leading record survives"
    );
    assert_eq!(got3[0].path, "b.rs");
    eprintln!("   PASS: truncated tail ignored, leading record OK\n");

    // ── 4. Block GC: referenced vs orphan ──
    eprintln!("[4/4] gc_orphans() with get_all_block_hashes()...");
    let blocks_dir = root.join("blocks");
    let idx_dir = root.join("index");
    let bs = proxygit_server::block_store::BlockStore::new(&blocks_dir).unwrap();
    let idx = proxygit_server::indexer::ProjectIndexer::new(&idx_dir).unwrap();

    let pid = uuid::Uuid::new_v4();

    let h_ref = blake3::hash(b"keep me");
    let h_orph = blake3::hash(b"delete me");
    bs.store_block(h_ref.as_bytes(), b"keep me").unwrap();
    bs.store_block(h_orph.as_bytes(), b"delete me").unwrap();

    let chunk = proxygit_common::cdc::ChunkResult {
        hash: h_ref,
        offset: 0,
        length: 7,
        data: b"keep me".to_vec(),
    };
    idx.ingest_chunks(&pid, "survivor.txt", &[chunk]).unwrap();

    let all_hashes = idx.get_all_block_hashes(&pid).unwrap();
    assert_eq!(all_hashes.len(), 1);
    assert_eq!(all_hashes[0], h_ref.to_hex().to_string());

    let referenced: std::collections::HashSet<String> = all_hashes.into_iter().collect();
    let deleted = bs.gc_orphans(&referenced).unwrap();
    assert_eq!(deleted, 1, "orphan block must be GC'd");
    assert!(
        bs.has_block(h_ref.as_bytes()),
        "referenced block must survive"
    );
    assert!(
        !bs.has_block(h_orph.as_bytes()),
        "orphan block must be deleted"
    );
    eprintln!("   PASS: GC deleted {deleted} orphan, kept referenced\n");

    std::fs::remove_dir_all(&root).unwrap_or(());
    eprintln!("=== ALL 4/4 AD-HOC CHECKS PASSED ===");
}
