//! # SimdMash
//!
//! This library provides two types of sequence sketches:
//! - the classic bottom-`s` mash;
//! - the newer bin-mash, returning the smallest hash in each of `s` bins.
//!
//! ## Hash function
//! All internal hashes are 32 bits. Either a forward-only hash or
//! reverse-complement-aware (canonical) hash can be used.
//!
//! *TODO:* Current we use (canonical) ntHash. This causes some hash-collisions
//! for `k <= 16`, [which can be avoided](https://curiouscoding.nl/posts/nthash/#is-nthash-injective-on-kmers).
//!
//! ## BinMash
//! For classic bottom-mash, evaluating the similarity is slow because a
//! merge-sort must be done between the two lists.
//!
//! BinMash solves this by partitioning the hashes into `s` partitions.
//! Previous methods partition into ranges of size `u32::MAX/s`, but here we
//! partition by remainder mod `s` instead.
//!
//! We find the smallest hash for each remainder as the sketch.
//! To compute the similarity, we can simply use the hamming distance between
//! two sketches, which is significantly faster.
//!
//! The bin-mash similarity has a very strong one-to-one correlation with the classic bottom-mash.
//!
//! ## Jaccard similarity
//! For the bottom-mash, we conceptually estimate similarity as follows:
//! 1. Find the smallest `s` distinct k-mer hashes in the union of two sketches.
//! 2. Return the fraction of these k-mers that occurs in both sketches.
//!
//! For the bin-mash, we simply return the fraction of partitions that have
//! the same k-mer for both sequences.
//!
//! ## Usage
//!
//! The main entrypoint of this library is the [`Masher`] object.
//! Construct it in either the forward or canonical variant, and give `k` and `s`.
//! Then call either [`Masher::bottom_mash`] or [`Masher::bin_mash`] on it, and use the
//! `similarity` functions on the returned [`BottomMash`] and [`BinMash`] objects.
//!
//! ```
//! use packed_seq::SeqVec;
//!
//! // Bottom s=10000 sketch of k=31-mers.
//! let k = 31;
//! let s = 10_000;
//!
//! // Use `new_rc` for a canonical version instead.
//! let masher = simd_mash::Masher::new(k, s);
//!
//! // Generate two random sequences of 2M characters.
//! let n = 2_000_000;
//! let seq1 = packed_seq::PackedSeqVec::random(n);
//! let seq2 = packed_seq::PackedSeqVec::random(n);
//!
//! // Bottom-mash variant
//!
//! let mash1: simd_mash::BottomMash = masher.bottom_mash(seq1.as_slice());
//! let mash2: simd_mash::BottomMash = masher.bottom_mash(seq2.as_slice());
//!
//! // Value between 0 and 1, estimating the fraction of shared k-mers.
//! let similarity = mash1.similarity(&mash2);
//!
//! // Bin-mash variant
//!
//! let mash1: simd_mash::BinMash = masher.bin_mash(seq1.as_slice());
//! let mash2: simd_mash::BinMash = masher.bin_mash(seq2.as_slice());
//!
//! // Value between 0 and 1, estimating the fraction of shared k-mers.
//! let similarity = mash1.similarity(&mash2);
//! ```
//!
//! ## Implementation notes
//!
//! This library works by partitioning the input sequence into 8 chunks,
//! and processing those in parallel using SIMD.
//! This is based on the [`packed-seq`](../packed_seq/index.html) and [`simd-minimizers`](../simd_minimizers/index.html) crates.
//!
//! For bottom-mash, the largest hash should be around `target = u32::MAX * s / n` (ignoring duplicates).
//! To ensure a branch-free algorithm, we first collect all hashes up to `bound = 1.5 * target`.
//! Then we sort the collected hashes. If there are at least `s` left after deduplicating, we return the bottom `s`.
//! Otherwise, we double the `1.5` multiplication factor and retry. This
//! factor is cached to make the sketching of multiple genomes more efficient.
//!
//! For bin-mash, we use the same approach, and increase the factor until we find a k-mer hash in every bucket.
//! In expectation, this needs to collect a fraction around `log(n) * s / n` of hashes, rather than `s / n`.
//! In practice this doesn't matter much, as the hashing of all input k-mers is the bottleneck,
//! and the sorting of the small sample of k-mers is relatively fast.
//!
//! For bin-mash we assign each element to its bucket via its remainder modulo `s`.
//! We compute this efficiently using [fast-mod](https://github.com/lemire/fastmod/blob/master/include/fastmod.h).
//!
//! ## Performance
//!
//! The sketching throughput of this library is around 2 seconds for a 3GB human genome
//! (once the scaling factor is large enough to avoid a second pass).
//! That's typically a few times faster than parsing a Fasta file.
//!
//! [BinDash](https://github.com/zhaoxiaofei/bindash) instead takes 180s (90x
//! more), when running on a single thread.
//!
//! Comparing sketches is relatively fast, but can become a bottleneck when there are many input sequences,
//! since the number of comparisons grows quadratically. In this case, prefer bin-mash.
//! As an example, when sketching 5MB bacterial genomes using `s=10000`, each sketch takes 4ms.
//! Comparing two sketches takes 1.6us.
//! This starts to be the dominant factor when the number of input sequences is more than 5000.
//!
//! TODO: Document `b`.

mod intrinsics;

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use packed_seq::{u32x8, Seq};
use simd_minimizers::private::nthash::NtHasher;
use tracing::debug;

enum BitSketch {
    B32(Vec<u32>),
    B16(Vec<u16>),
    B8(Vec<u8>),
    B1(Vec<u64>),
}

impl BitSketch {
    fn new(b: usize, vals: Vec<u32>) -> Self {
        match b {
            32 => BitSketch::B32(vals),
            16 => BitSketch::B16(vals.into_iter().map(|x| x as u16).collect()),
            8 => BitSketch::B8(vals.into_iter().map(|x| x as u8).collect()),
            1 => BitSketch::B1({
                assert_eq!(vals.len() % 64, 0);
                vals.chunks_exact(64)
                    .map(|xs| {
                        xs.iter()
                            .enumerate()
                            .fold(0u64, |bits, (i, x)| bits | (((x & 1) as u64) << i))
                    })
                    .collect()
            }),
            _ => panic!("Unsupported bit width. Must be 1 or 8 or 16 or 32."),
        }
    }
}

/// A sketch containing the `s` smallest k-mer hashes.
pub struct BottomMash {
    rc: bool,
    k: usize,
    b: usize,
    bottom: BitSketch,
}

impl BottomMash {
    /// Compute the similarity between two `BottomMash`es.
    pub fn similarity(&self, other: &Self) -> f32 {
        assert_eq!(self.rc, other.rc);
        assert_eq!(self.k, other.k);
        assert_eq!(self.b, other.b);
        match (&self.bottom, &other.bottom) {
            (BitSketch::B32(a), BitSketch::B32(b)) => Self::inner_similarity(a, b),
            (BitSketch::B16(a), BitSketch::B16(b)) => Self::inner_similarity(a, b),
            (BitSketch::B8(a), BitSketch::B8(b)) => Self::inner_similarity(a, b),
            _ => panic!("Bit width mismatch"),
        }
    }

    fn inner_similarity<T: Eq + Ord>(a: &Vec<T>, b: &Vec<T>) -> f32 {
        assert_eq!(a.len(), b.len());
        let mut intersection_size = 0;
        let mut union_size = 0;
        let mut i = 0;
        let mut j = 0;
        while union_size < a.len() {
            intersection_size += (a[i] == b[j]) as usize;
            let di = (a[i] <= b[j]) as usize;
            let dj = (a[i] >= b[j]) as usize;
            i += di;
            j += dj;
            union_size += 1;
        }

        return intersection_size as f32 / a.len() as f32;
    }
}

/// A sketch containing the smallest k-mer hash for each remainder mod `s`.
pub struct BinMash {
    rc: bool,
    k: usize,
    b: usize,
    bins: BitSketch,
}

impl BinMash {
    /// Compute the similarity between two `BinMash`es.
    pub fn similarity(&self, other: &Self) -> f32 {
        assert_eq!(self.rc, other.rc);
        assert_eq!(self.k, other.k);
        assert_eq!(self.b, other.b);
        match (&self.bins, &other.bins) {
            (BitSketch::B32(a), BitSketch::B32(b)) => Self::inner_similarity(a, b),
            (BitSketch::B16(a), BitSketch::B16(b)) => Self::inner_similarity(a, b),
            (BitSketch::B8(a), BitSketch::B8(b)) => Self::inner_similarity(a, b),
            (BitSketch::B1(a), BitSketch::B1(b)) => Self::b1_similarity(a, b),
            _ => panic!("Bit width mismatch"),
        }
    }
    fn inner_similarity<T: Eq>(a: &Vec<T>, b: &Vec<T>) -> f32 {
        assert_eq!(a.len(), b.len());
        std::iter::zip(a, b)
            .map(|(a, b)| (a == b) as u32)
            .sum::<u32>() as f32
            / a.len() as f32
    }

    fn b1_similarity(a: &Vec<u64>, b: &Vec<u64>) -> f32 {
        assert_eq!(a.len(), b.len());
        let f = std::iter::zip(a, b)
            .map(|(a, b)| (*a ^ *b).count_zeros())
            .sum::<u32>() as f32
            / (64 * a.len()) as f32;
        2. * f - 1.
    }
}

/// An object containing the mash parameters.
///
/// Contains internal state to optimize the implementation when sketching multiple similar sequences.
pub struct Masher<const RC: bool> {
    k: usize,
    s: usize,
    b: usize,

    factor: AtomicUsize,
}

impl Masher<false> {
    /// Construct a new forward-only `Masher` object.
    pub fn new(k: usize, s: usize, b: usize) -> Self {
        Masher::<false> {
            k,
            s,
            b,
            factor: 2.into(),
        }
    }
}

impl Masher<true> {
    /// Construct a new reverse-complement-aware `Masher` object.
    pub fn new_rc(k: usize, s: usize, b: usize) -> Self {
        Masher::<true> {
            k,
            s,
            b,
            factor: 2.into(),
        }
    }
}

impl<const RC: bool> Masher<RC> {
    /// Return the `s` smallest `u32` k-mer hashes.
    pub fn bottom_mash<'s, S: Seq<'s>>(&self, seq: S) -> BottomMash {
        // Iterate all kmers and compute 32bit nthashes.
        let n = seq.len();
        let mut out = vec![];
        loop {
            let target = u32::MAX as usize / n * self.s;
            let bound =
                (target.saturating_mul(self.factor.load(SeqCst))).min(u32::MAX as usize) as u32;

            collect_up_to_bound::<RC, S>(seq, self.k, bound, &mut out);

            if bound == u32::MAX || out.len() >= self.s {
                out.sort_unstable();
                out.dedup();
                if bound == u32::MAX || out.len() >= self.s {
                    out.resize(self.s, u32::MAX);

                    break BottomMash {
                        rc: RC,
                        k: self.k,
                        b: self.b,
                        bottom: BitSketch::new(self.b, out),
                    };
                }
            }
            self.factor
                .fetch_add((self.factor.load(SeqCst) + 1) / 2, SeqCst);
            debug!("Increase factor to {}", self.factor.load(SeqCst));
        }
    }

    /// Split the hashes into `s` buckets and return the smallest hash in each bucket.
    ///
    /// Buckets are determined via the remainder mod `s`.
    pub fn bin_mash<'s, S: Seq<'s>>(&self, seq: S) -> BinMash {
        // Iterate all kmers and compute 32bit nthashes.
        let n = seq.len();
        let mut out = vec![];
        let mut bins = vec![u32::MAX; self.s];
        loop {
            let target = u32::MAX as usize / n * self.s;
            let bound =
                (target.saturating_mul(self.factor.load(SeqCst))).min(u32::MAX as usize) as u32;

            collect_up_to_bound::<RC, S>(seq, self.k, bound, &mut out);

            if bound == u32::MAX || out.len() >= self.s {
                let m = FM32::new(self.s as u32);
                for &hash in &out {
                    let bin = m.fastmod(hash);
                    bins[bin] = bins[bin].min(hash);
                }
                let mut empty = 0;
                for &x in &bins {
                    if x == u32::MAX {
                        empty += 1;
                    }
                }
                if bound == u32::MAX || empty == 0 {
                    break BinMash {
                        rc: RC,
                        k: self.k,
                        b: self.b,
                        bins: BitSketch::new(
                            self.b,
                            bins.into_iter().map(|x| m.fastdiv(x) as u32).collect(),
                        ),
                    };
                }
            }
            self.factor
                .fetch_add((self.factor.load(SeqCst) + 1) / 2, SeqCst);
            debug!("Increase factor to {}", self.factor.load(SeqCst));
        }
    }
}

fn collect_up_to_bound<'s, const RC: bool, S: Seq<'s>>(
    seq: S,
    k: usize,
    bound: u32,
    out: &mut Vec<u32>,
) {
    let simd_bound = u32x8::splat(bound);

    let (hashes_head, hashes_tail) =
        simd_minimizers::private::nthash::nthash_seq_simd::<RC, S, NtHasher>(seq, k, 1);

    out.clear();
    let mut write_idx = 0;
    for hashes in hashes_head {
        let mask = hashes.cmp_lt(simd_bound);
        if write_idx + 8 >= out.len() {
            out.resize(write_idx * 3 / 2 + 8, 0);
        }
        unsafe { intrinsics::append_from_mask(hashes, mask, out, &mut write_idx) };
    }

    out.resize(write_idx, 0);

    for hash in hashes_tail {
        if hash <= bound {
            out.push(hash);
        }
    }
}

/// FastMod32, using the low 32 bits of the hash.
/// Taken from https://github.com/lemire/fastmod/blob/master/include/fastmod.h
#[derive(Copy, Clone, Debug)]
struct FM32 {
    d: u64,
    m: u64,
}
impl FM32 {
    fn new(d: u32) -> Self {
        Self {
            d: d as u64,
            m: u64::MAX / d as u64 + 1,
        }
    }
    fn fastmod(self, h: u32) -> usize {
        let lowbits = self.m.wrapping_mul(h as u64);
        ((lowbits as u128 * self.d as u128) >> 64) as usize
    }
    fn fastdiv(self, h: u32) -> usize {
        ((self.m as u128 * h as u128) >> 64) as u32 as usize
    }
}

#[cfg(test)]
#[test]
fn test() {
    use packed_seq::SeqVec;
    let b = 16;

    let k = 31;
    for n in 31..100 {
        let s = n - k + 1;
        let seq = packed_seq::PackedSeqVec::random(n);
        let masher = crate::Masher::new(k, s, b);
        let mash = masher.bottom_mash(seq.as_slice());
        let BitSketch::B16(bottom) = mash.bottom else {
            panic!()
        };
        assert_eq!(bottom.len(), s);
        assert!(bottom.is_sorted());

        let s = s.min(10);
        let seq = packed_seq::PackedSeqVec::random(n);
        let masher = crate::Masher::new(k, s, b);
        let mash = masher.bottom_mash(seq.as_slice());
        let BitSketch::B16(bottom) = mash.bottom else {
            panic!()
        };
        assert_eq!(bottom.len(), s);
        assert!(bottom.is_sorted());
    }
}

#[cfg(test)]
#[test]
fn rc() {
    use packed_seq::SeqVec;

    let b = 32;
    for k in (0..10).map(|_| rand::random_range(1..100)) {
        for n in (0..10).map(|_| rand::random_range(k..1000)) {
            for s in (0..10).map(|_| rand::random_range(0..n - k + 1)) {
                let seq = packed_seq::AsciiSeqVec::random(n);
                let masher = crate::Masher::new_rc(k, s, b);
                let mash = masher.bottom_mash(seq.as_slice());
                let BitSketch::B32(bottom) = mash.bottom else {
                    panic!()
                };
                assert_eq!(bottom.len(), s);
                assert!(bottom.is_sorted());

                let seq_rc = packed_seq::AsciiSeqVec::from_ascii(
                    &seq.seq
                        .iter()
                        .rev()
                        .map(|c| packed_seq::complement_char(*c))
                        .collect::<Vec<_>>(),
                );

                let mash_rc = masher.bottom_mash(seq_rc.as_slice());
                let BitSketch::B32(bottom_rc) = mash_rc.bottom else {
                    panic!()
                };
                assert_eq!(bottom, bottom_rc);
            }
        }
    }
}
