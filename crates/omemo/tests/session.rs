//! Full sessions between two simulated devices, for both OMEMO versions.

use omemo::memory::MemoryStore;
use omemo::{Context, RemoteBundle, Version, fingerprint, payload};

struct Device {
    ctx: Context,
    store: MemoryStore,
    id: u32,
}

fn device(version: Version, id: u32) -> Device {
    let store = MemoryStore::new(id);
    let ctx = Context::new(version, Box::new(store.clone())).unwrap();
    let (public, private) = ctx.generate_identity_key_pair().unwrap();
    store.set_identity(public, private);
    let mut s = store.clone();
    use omemo::OmemoStore;
    for (pid, rec) in ctx.generate_pre_keys(1, 25).unwrap() {
        s.store_pre_key(pid, &rec).unwrap();
    }
    let spk = ctx.generate_signed_pre_key(1, 1_700_000_000_000).unwrap();
    s.store_signed_pre_key(1, &spk).unwrap();
    Device { ctx, store, id }
}

fn bundle_of(d: &Device, pre_key_index: usize) -> RemoteBundle {
    use omemo::OmemoStore;
    let mut s = d.store.clone();
    let spk = s.load_signed_pre_key(1).unwrap().unwrap();
    let pre: Vec<(u32, Vec<u8>)> = d
        .store
        .pre_key_ids()
        .into_iter()
        .map(|id| (id, s.load_pre_key(id).unwrap().unwrap()))
        .collect();
    let own = d.ctx.own_bundle(&spk, &pre).unwrap();
    let (pk_id, pk) = own.pre_keys[pre_key_index].clone();
    RemoteBundle {
        identity_key: own.identity_key,
        signed_pre_key_id: own.signed_pre_key_id,
        signed_pre_key: own.signed_pre_key,
        signed_pre_key_signature: own.signed_pre_key_signature,
        pre_key_id: pk_id,
        pre_key: pk,
    }
}

fn conversation(version: Version) {
    let alice = device(version, 1001);
    let bob = device(version, 2002);

    // Wire formats per version.
    let b = bundle_of(&bob, 3);
    match version {
        Version::Legacy => {
            assert_eq!(b.identity_key.len(), 33);
            assert_eq!(b.signed_pre_key.len(), 33);
            assert_eq!(b.pre_key.len(), 33);
        }
        Version::Omemo2 => {
            assert_eq!(b.identity_key.len(), 32);
            assert_eq!(b.signed_pre_key.len(), 32);
            assert_eq!(b.pre_key.len(), 32);
        }
    }
    assert_eq!(b.signed_pre_key_signature.len(), 64);
    assert!(fingerprint::fingerprint(version, &b.identity_key).is_some());

    // A tampered signature must be rejected.
    let mut forged = b.clone();
    forged.signed_pre_key_signature[5] ^= 1;
    assert!(alice.ctx.process_bundle("bob@x", bob.id, &forged).is_err());

    alice.ctx.process_bundle("bob@x", bob.id, &b).unwrap();
    assert!(alice.ctx.has_session("bob@x", bob.id));

    // Alice → Bob: payload + key exchange.
    let sealed = match version {
        Version::Omemo2 => payload::omemo2::seal(b"hello bob"),
        Version::Legacy => payload::legacy::seal(b"hello bob"),
    };
    let (wire, kex) = alice.ctx.encrypt("bob@x", bob.id, &sealed.key_material).unwrap();
    assert!(kex, "first message must be a key exchange");

    let dec = bob.ctx.decrypt("alice@x", alice.id, &wire, true).unwrap();
    assert_eq!(dec.used_pre_key, Some(b.pre_key_id));
    assert!(
        !bob.store.pre_key_ids().contains(&b.pre_key_id),
        "used pre key is removed"
    );
    let body = match version {
        Version::Omemo2 => payload::omemo2::open(&dec.plaintext, &sealed.ciphertext).unwrap(),
        Version::Legacy => {
            payload::legacy::open(&dec.plaintext, sealed.iv.as_deref().unwrap(), &sealed.ciphertext).unwrap()
        }
    };
    assert_eq!(body, b"hello bob");

    // Replaying the key exchange must not decrypt again.
    assert!(bob.ctx.decrypt("alice@x", alice.id, &wire, true).is_err());

    // Bob → Alice: normal message on the established session.
    let (reply, kex) = bob.ctx.encrypt("alice@x", alice.id, &[42u8; 48]).unwrap();
    assert!(!kex);
    let dec = alice.ctx.decrypt("bob@x", bob.id, &reply, false).unwrap();
    assert_eq!(dec.plaintext, vec![42u8; 48]);
    assert_eq!(dec.used_pre_key, None);

    // Several more in both directions, delivered out of order.
    let m1 = alice.ctx.encrypt("bob@x", bob.id, b"one").unwrap().0;
    let m2 = alice.ctx.encrypt("bob@x", bob.id, b"two").unwrap().0;
    assert_eq!(
        bob.ctx.decrypt("alice@x", alice.id, &m2, false).unwrap().plaintext,
        b"two"
    );
    assert_eq!(
        bob.ctx.decrypt("alice@x", alice.id, &m1, false).unwrap().plaintext,
        b"one"
    );

    // Bob recorded Alice's identity.
    assert!(bob.store.remote_identity("alice@x", alice.id as i32).is_some());
}

#[test]
fn omemo2_session() {
    conversation(Version::Omemo2);
}

#[test]
fn legacy_session() {
    conversation(Version::Legacy);
}

#[test]
fn versions_do_not_interoperate() {
    let alice = device(Version::Omemo2, 1);
    let bob = device(Version::Legacy, 2);
    let b = bundle_of(&bob, 0);
    // A legacy bundle (33-byte keys) can't be used by an OMEMO:2 context.
    assert!(alice.ctx.process_bundle("bob@x", 2, &b).is_err());
}
