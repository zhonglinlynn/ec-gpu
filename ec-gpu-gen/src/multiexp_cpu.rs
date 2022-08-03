#![allow(missing_docs)]
use std::convert::TryInto;
use std::io;
use std::iter;
use std::ops::AddAssign;
use std::sync::Arc;
use bitvec::prelude::{BitVec, Lsb0};

//use group::{prime::PrimeCurveAffine, Group};
use pairing_ce::ff::{Field, PrimeField, ScalarEngine};
use pairing_ce::{Engine, CurveAffine, CurveProjective};

use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use crate::error::EcError;
use crate::threadpool::{Waiter, Worker};

/// An object that builds a source of bases.
pub trait SourceBuilder<G: CurveAffine>: Send + Sync + 'static + Clone {
    type Source: Source<G>;

    #[allow(clippy::wrong_self_convention)]
    fn new(self) -> Self::Source;
    fn get(self) -> (Arc<Vec<G>>, usize);
}

/// A source of bases, like an iterator.
pub trait Source<G: CurveAffine> {
    /// Parses the element from the source. Fails if the point is at infinity.
    fn add_assign_mixed(&mut self, to: &mut <G as CurveAffine>::Projective) -> Result<(), EcError>;

    /// Skips `amt` elements from the source, avoiding deserialization.
    fn skip(&mut self, amt: usize) -> Result<(), EcError>;
}

impl<G: CurveAffine> SourceBuilder<G> for (Arc<Vec<G>>, usize) {
    type Source = (Arc<Vec<G>>, usize);

    fn new(self) -> (Arc<Vec<G>>, usize) {
        (self.0.clone(), self.1)
    }

    fn get(self) -> (Arc<Vec<G>>, usize) {
        (self.0.clone(), self.1)
    }
}

impl<G: CurveAffine> Source<G> for (Arc<Vec<G>>, usize) {
    fn add_assign_mixed(&mut self, to: &mut <G as CurveAffine>::Projective) -> Result<(), EcError> {
        if self.0.len() <= self.1 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Expected more bases from source.",
            )
            .into());
        }

        if self.0[self.1].is_zero().into() {
            return Err(EcError::Simple(
                "Encountered an identity element in the CRS.",
            ));
        }

        to.add_assign(&self.0[self.1]);

        self.1 += 1;

        Ok(())
    }

    fn skip(&mut self, amt: usize) -> Result<(), EcError> {
        if self.0.len() <= self.1 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Expected more bases from source.",
            )
            .into());
        }

        self.1 += amt;

        Ok(())
    }
}

pub trait QueryDensity: Sized {
    /// Returns whether the base exists.
    type Iter: Iterator<Item = bool>;

    fn iter(self) -> Self::Iter;
    fn get_query_size(self) -> Option<usize>;
    fn generate_exps<E: Engine>(
        self,
        exponents: Arc<Vec<<<E as ScalarEngine>::Fr as PrimeField>::Repr>>,
    ) -> Arc<Vec<<<E as ScalarEngine>::Fr as PrimeField>::Repr>>;
}

#[derive(Clone)]
pub struct FullDensity;

impl AsRef<FullDensity> for FullDensity {
    fn as_ref(&self) -> &FullDensity {
        self
    }
}

impl<'a> QueryDensity for &'a FullDensity {
    type Iter = iter::Repeat<bool>;

    fn iter(self) -> Self::Iter {
        iter::repeat(true)
    }

    fn get_query_size(self) -> Option<usize> {
        None
    }

    fn generate_exps<E: Engine>(
        self,
        exponents: Arc<Vec<<<E as ScalarEngine>::Fr as PrimeField>::Repr>>,
    ) -> Arc<Vec<<<E as ScalarEngine>::Fr as PrimeField>::Repr>> {
        exponents
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DensityTracker {
    pub bv: BitVec,
    pub total_density: usize,
}

impl<'a> QueryDensity for &'a DensityTracker {
    type Iter = bitvec::slice::BitValIter<'a, Lsb0, usize>;

    fn iter(self) -> Self::Iter {
        self.bv.iter().by_val()
    }

    fn get_query_size(self) -> Option<usize> {
        Some(self.bv.len())
    }

    fn generate_exps<E: Engine>(
        self,
        exponents: Arc<Vec<<<E as ScalarEngine>::Fr as PrimeField>::Repr>>,
    ) -> Arc<Vec<<<E as ScalarEngine>::Fr as PrimeField>::Repr>> {
        let exps: Vec<_> = exponents
            .iter()
            .zip(self.bv.iter())
            .filter_map(|(&e, d)| if *d { Some(e) } else { None })
            .collect();

        Arc::new(exps)
    }
}

impl DensityTracker {
    pub fn new() -> DensityTracker {
        DensityTracker {
            bv: BitVec::new(),
            total_density: 0,
        }
    }

    pub fn add_element(&mut self) {
        self.bv.push(false);
    }

    pub fn inc(&mut self, idx: usize) {
        if !self.bv.get(idx).unwrap() {
            self.bv.set(idx, true);
            self.total_density += 1;
        }
    }

    pub fn get_total_density(&self) -> usize {
        self.total_density
    }

    /// Extend by concatenating `other`. If `is_input_density` is true, then we are tracking an input density,
    /// and other may contain a redundant input for the `One` element. Coalesce those as needed and track the result.
    pub fn extend(&mut self, other: Self, is_input_density: bool) {
        if other.bv.is_empty() {
            // Nothing to do if other is empty.
            return;
        }

        if self.bv.is_empty() {
            // If self is empty, assume other's density.
            self.total_density = other.total_density;
            self.bv = other.bv;
            return;
        }

        if is_input_density {
            // Input densities need special handling to coalesce their first inputs.

            if other.bv[0] {
                // If other's first bit is set,
                if self.bv[0] {
                    // And own first bit is set, then decrement total density so the final sum doesn't overcount.
                    self.total_density -= 1;
                } else {
                    // Otherwise, set own first bit.
                    self.bv.set(0, true);
                }
            }
            // Now discard other's first bit, having accounted for it above, and extend self by remaining bits.
            self.bv.extend(other.bv.iter().skip(1));
        } else {
            // Not an input density, just extend straightforwardly.
            self.bv.extend(other.bv);
        }

        // Since any needed adjustments to total densities have been made, just sum the totals and keep the sum.
        self.total_density += other.total_density;
    }
}

// Right shift the repr of a field element by `n` bits.
fn shr(le_bytes: &mut [u8], mut n: u32) {
    if n >= 8 * le_bytes.len() as u32 {
        le_bytes.iter_mut().for_each(|byte| *byte = 0);
        return;
    }

    // Shift each full byte towards the least significant end.
    while n >= 8 {
        let mut replacement = 0;
        for byte in le_bytes.iter_mut().rev() {
            std::mem::swap(&mut replacement, byte);
        }
        n -= 8;
    }

    // Starting at the most significant byte, shift the byte's `n` least significant bits into the
    // `n` most signficant bits of the next byte.
    if n > 0 {
        let mut shift_in = 0;
        for byte in le_bytes.iter_mut().rev() {
            // Copy the byte's `n` least significant bits.
            let shift_out = *byte << (8 - n);
            // Shift the byte by `n` bits; zeroing its `n` most significant bits.
            *byte >>= n;
            // Replace the `n` most significant bits with the bits shifted out of the previous byte.
            *byte |= shift_in;
            shift_in = shift_out;
        }
    }
}

fn multiexp_inner<Q, D, G, S>(
    bases: S,
    density_map: D,
    exponents: Arc<Vec<<G::Scalar as PrimeField>::Repr>>,
    c: u32,
) -> Result<<G as CurveAffine>::Projective, EcError>
where
    for<'a> &'a Q: QueryDensity,
    D: Send + Sync + 'static + Clone + AsRef<Q>,
    G: CurveAffine,
    S: SourceBuilder<G>,
{
    // Perform this region of the multiexp
    let this = move |bases: S,
                     density_map: D,
                     exponents: Arc<Vec<<G::Scalar as PrimeField>::Repr>>,
                     skip: u32|
          -> Result<_, EcError> {
        // Accumulate the result
        let mut acc = G::Projective::zero();

        // Build a source for the bases
        let mut bases = bases.new();

        // Create space for the buckets
        let mut buckets = vec![<G as CurveAffine>::Projective::zero(); (1 << c) - 1];

        let zero = G::Scalar::zero().into_repr();
        let one = G::Scalar::one().into_repr();

        // only the first round uses this
        let handle_trivial = skip == 0;

        // Sort the bases into buckets
        for (&exp, density) in exponents.iter().zip(density_map.as_ref().iter()) {
            if density {
                if exp.as_ref() == zero.as_ref() {
                    bases.skip(1)?;
                } else if exp.as_ref() == one.as_ref() {
                    if handle_trivial {
                        bases.add_assign_mixed(&mut acc)?;
                    } else {
                        bases.skip(1)?;
                    }
                } else {
                    let mut exp = exp;
                    shr(exp.as_mut(), skip);
                    let exp = u64::from_le_bytes(exp.as_ref()[..8].try_into().unwrap()) % (1 << c);

                    if exp != 0 {
                        bases.add_assign_mixed(&mut buckets[(exp - 1) as usize])?;
                    } else {
                        bases.skip(1)?;
                    }
                }
            }
        }

        // Summation by parts
        // e.g. 3a + 2b + 1c = a +
        //                    (a) + b +
        //                    ((a) + b) + c
        let mut running_sum = G::Projective::zero();
        for exp in buckets.into_iter().rev() {
            running_sum.add_assign(&exp);
            acc.add_assign(&running_sum);
        }

        Ok(acc)
    };

    let parts = (0..<G::Scalar as PrimeField>::NUM_BITS)
        .into_par_iter()
        .step_by(c as usize)
        .map(|skip| this(bases.clone(), density_map.clone(), exponents.clone(), skip))
        .collect::<Vec<Result<_, _>>>();

    parts.into_iter().rev().try_fold(
        <G as CurveAffine>::Projective::zero(),
        |mut acc, part| {
            for _ in 0..c {
                acc.double();
            }
            acc.add_assign(&part?);
            Ok(acc)
        },
    )
}

/// Perform multi-exponentiation. The caller is responsible for ensuring the
/// query size is the same as the number of exponents.
pub fn multiexp_cpu<'b, Q, D, G, E, S>(
    pool: &Worker,
    bases: S,
    density_map: D,
    exponents: Arc<Vec<<G::Scalar as PrimeField>::Repr>>,
) -> Waiter<Result<<G as CurveAffine>::Projective, EcError>>
where
    for<'a> &'a Q: QueryDensity,
    D: Send + Sync + 'static + Clone + AsRef<Q>,
    G: CurveAffine,
    E: Engine<Fr = G::Scalar>,
    S: SourceBuilder<G>,
{
    let c = if exponents.len() < 32 {
        3u32
    } else {
        (f64::from(exponents.len() as u32)).ln().ceil() as u32
    };

    if let Some(query_size) = density_map.as_ref().get_query_size() {
        // If the density map has a known query size, it should not be
        // inconsistent with the number of exponents.
        assert_eq!(query_size, exponents.len());
    }

    pool.compute(move || multiexp_inner(bases, density_map, exponents, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    //use pairing_ce::bn256::Bn256;
    use pairing_ce::compact_bn256::Bn256;
    //use pairing_ce::bls12_381::Bls12 as Bn256;


    use rand::{thread_rng, Rng, Rand};
    use rand_core::SeedableRng;
    use rand_xorshift::XorShiftRng;

    #[test]
    fn multiexp() {
        fn naive_multiexp<G: CurveAffine>(
            bases: Arc<Vec<G>>,
            exponents: &[G::Scalar],
        ) -> G::Projective {
            assert_eq!(bases.len(), exponents.len());

            let mut acc = G::Projective::zero();

            for (base, exp) in bases.iter().zip(exponents.iter()) {
                acc.add_assign(&base.mul(*exp));
            }

            acc
        }

        const SAMPLES: usize = 1 << 14;

        let rng = &mut rand::thread_rng();
        let rng_1 = &mut thread_rng();

        let v: Vec<<Bn256 as ScalarEngine>::Fr> = (0..SAMPLES)
            .map(|_| rng_1.gen()).collect::<Vec<_>>();


        let g = Arc::new(
            (0..SAMPLES)
                .map(|_| <Bn256 as Engine>::G1::rand(&mut *rng).into_affine())
                .collect::<Vec<_>>(),
        );

        let now = std::time::Instant::now();
        let naive = naive_multiexp(g.clone(), &v);
        println!("Naive: {}", now.elapsed().as_millis());

        let now = std::time::Instant::now();
        let pool = Worker::new();

        let v = Arc::new(v.into_iter().map(|fr| fr.to_repr()).collect());
        let fast = multiexp_cpu::<_, _, _, Bn256, _>(&pool, (g, 0), FullDensity, v)
            .wait()
            .unwrap();

        println!("Fast: {}", now.elapsed().as_millis());

        assert_eq!(naive, fast);
    }
}
