#![feature(test)]

extern crate test;

use {
    log::*,
    rand::{thread_rng, Rng},
    solana_perf::{
        packet::{to_packet_batches, Packet, PacketBatch},
        recycler::Recycler,
        sigverify,
        test_tx::{test_multisig_tx, test_tx},
    },
    test::Bencher,
};

const NUM: usize = 256;

#[bench]
fn bench_sigverify_simple(bencher: &mut Bencher) {
    let tx = test_tx();
    let num_packets = NUM;

    // generate packet vector
    let mut batches = to_packet_batches(
        &std::iter::repeat(tx).take(num_packets).collect::<Vec<_>>(),
        128,
    );

    let recycler = Recycler::default();
    let recycler_out = Recycler::default();
    // verify packets
    bencher.iter(|| {
        sigverify::ed25519_verify(&mut batches, &recycler, &recycler_out, false, num_packets);
    })
}

#[bench]
#[ignore]
fn bench_sigverify_uneven(bencher: &mut Bencher) {
    solana_logger::setup();
    let simple_tx = test_tx();
    let multi_tx = test_multisig_tx();
    let mut tx;

    let num_packets = NUM * 50;
    let mut num_valid = 0;
    let mut current_packets = 0;
    // generate packet vector
    let mut batches = vec![];
    while current_packets < num_packets {
        let mut len: usize = thread_rng().gen_range(1, 128);
        current_packets += len;
        if current_packets > num_packets {
            len -= current_packets - num_packets;
            current_packets = num_packets;
        }
        let mut batch = PacketBatch::with_capacity(len);
        batch.resize(len, Packet::default());
        for packet in batch.iter_mut() {
            if thread_rng().gen_ratio(1, 2) {
                tx = simple_tx.clone();
            } else {
                tx = multi_tx.clone();
            };
            Packet::populate_packet(packet, None, &tx).expect("serialize request");
            if thread_rng().gen_ratio((num_packets - NUM) as u32, num_packets as u32) {
                packet.meta.set_discard(true);
            } else {
                num_valid += 1;
            }
        }
        batches.push(batch);
    }
    info!("num_packets: {} valid: {}", num_packets, num_valid);

    let recycler = Recycler::default();
    let recycler_out = Recycler::default();
    // verify packets
    bencher.iter(|| {
        sigverify::ed25519_verify(&mut batches, &recycler, &recycler_out, false, num_packets);
    })
}

#[bench]
fn bench_get_offsets(bencher: &mut Bencher) {
    let tx = test_tx();

    // generate packet vector
    let mut batches =
        to_packet_batches(&std::iter::repeat(tx).take(1024).collect::<Vec<_>>(), 1024);

    let recycler = Recycler::default();
    // verify packets
    bencher.iter(|| {
        let _ans = sigverify::generate_offsets(&mut batches, &recycler, false);
    })
}
