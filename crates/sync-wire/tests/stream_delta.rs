use risunest_sync_wire::{
    delta::{Base, Recipe},
    hash, stream_delta, WireError,
};
use std::io::{Cursor, Read, Seek, SeekFrom};

struct Bounded(Cursor<Vec<u8>>);
impl Read for Bounded {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        assert!(bytes.len() <= 65536, "file IO must stay bounded");
        self.0.read(bytes)
    }
}
impl Seek for Bounded {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(position)
    }
}
fn identity(bytes: &[u8]) -> Base {
    Base {
        hash: hash(bytes),
        size: bytes.len() as u64,
    }
}
fn synthetic(size: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}
#[test]
fn stream_profile_exact_insert_delete_reorder_multiple_bases_and_bounded_io() {
    let a = synthetic(512 * 1024, 17);
    let b = synthetic(512 * 1024, 39);
    let new = [
        &b[65536..],
        b"new opaque bytes\0\xff",
        &a[130..400000],
        &b[..65536],
    ]
    .concat();
    let identities = vec![identity(&a), identity(&b)];
    let mut sources = vec![Bounded(Cursor::new(a)), Bounded(Cursor::new(b))];
    let mut target = Bounded(Cursor::new(new.clone()));
    let recipe = stream_delta::create(
        &mut sources,
        &identities,
        &mut target,
        identity(&new),
        || Ok(()),
    )
    .unwrap();
    let encoded = stream_delta::encode(&recipe).unwrap();
    assert!(encoded.len() < 16 * 1024);
    assert!(Recipe::decode(&encoded).is_err());
    let decoded = stream_delta::decode(&encoded).unwrap();
    let mut result = Vec::new();
    stream_delta::apply(&decoded, &mut sources, &mut result, || Ok(())).unwrap();
    assert_eq!(result, new);
    for n in 0..encoded.len() {
        assert!(stream_delta::decode(&encoded[..n]).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(stream_delta::decode(&trailing).is_err());
    let mut calls = 0;
    assert_eq!(
        stream_delta::apply(&decoded, &mut sources, &mut Vec::new(), || {
            calls += 1;
            if calls > 2 {
                Err(WireError("cancelled"))
            } else {
                Ok(())
            }
        })
        .unwrap_err()
        .0,
        "cancelled"
    );
    sources[0].0.get_mut()[0] ^= 1;
    assert_eq!(
        stream_delta::apply(&decoded, &mut sources, &mut Vec::new(), || Ok(()))
            .unwrap_err()
            .0,
        "base-hash-mismatch"
    );
}
#[test]
fn large_file_small_edit_uses_small_recipe_both_directions() {
    let old = synthetic(17 * 1024 * 1024, 91);
    let mut new = old.clone();
    new.splice(
        8 * 1024 * 1024 + 93..8 * 1024 * 1024 + 97,
        b"inserted".iter().copied(),
    );
    for (a, b) in [(&old, &new), (&new, &old)] {
        let mut source = [Cursor::new(a)];
        let recipe = stream_delta::create(
            &mut source,
            &[identity(a)],
            &mut Cursor::new(b),
            identity(b),
            || Ok(()),
        )
        .unwrap();
        assert!(stream_delta::encode(&recipe).unwrap().len() < 8192);
        assert!(recipe.validate().is_err());
        let mut result = Vec::new();
        stream_delta::apply(&recipe, &mut source, &mut result, || Ok(())).unwrap();
        assert_eq!(&result, b);
    }
}
