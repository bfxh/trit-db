//! Batch (vectorized) three-valued predicates and as-of joins.
//!
//! These are the M10 primitives that connect [`tritdb_tritvec`] to the
//! executor. The row-at-a-time `eval` path is unchanged: the batch path is
//! opt-in and is validated against the scalar truth tables (predicates) and a
//! brute-force scan (as-of joins).

use std::fmt;

use tritdb_tritvec::TritVecError;
pub use tritdb_tritvec::{TritVec, K3};

/// Errors from batch evaluation and as-of joins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchError {
    /// A predicate atom refers to a column that was not supplied.
    AtomOutOfRange {
        /// Requested atom index.
        index: usize,
        /// Number of supplied atom columns.
        atoms: usize,
    },
    /// Two atom vectors have different lengths.
    LengthMismatch {
        /// Left length.
        expected: usize,
        /// Right length.
        actual: usize,
    },
    /// An `AND`/`OR` term list was empty.
    EmptyTerms,
    /// The right side of an as-of join is not sorted by time.
    UnorderedRight {
        /// Index of the out-of-order row.
        index: usize,
        /// Previous timestamp.
        previous: i64,
        /// Current timestamp.
        current: i64,
    },
    /// A vector operation failed for another reason.
    Vector(TritVecError),
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchError::AtomOutOfRange { index, atoms } => {
                write!(
                    formatter,
                    "predicate atom {index} is outside {atoms} columns"
                )
            }
            BatchError::LengthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "batch length mismatch: expected {expected}, got {actual}"
                )
            }
            BatchError::EmptyTerms => formatter.write_str("AND/OR needs at least one term"),
            BatchError::UnorderedRight {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "as-of right side is not sorted at row {index}: {previous} then {current}"
            ),
            BatchError::Vector(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for BatchError {}

/// A three-valued predicate over atom columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriPredicate {
    /// Column `index` of the atom set.
    Atom(usize),
    /// `NOT`.
    Not(Box<TriPredicate>),
    /// `AND` (an empty list is rejected).
    And(Vec<TriPredicate>),
    /// `OR` (an empty list is rejected).
    Or(Vec<TriPredicate>),
}

/// Evaluates `predicate` element-wise over the atom columns.
pub fn eval_predicate(predicate: &TriPredicate, atoms: &[TritVec]) -> Result<TritVec, BatchError> {
    match predicate {
        TriPredicate::Atom(index) => atoms
            .get(*index)
            .cloned()
            .ok_or(BatchError::AtomOutOfRange {
                index: *index,
                atoms: atoms.len(),
            }),
        TriPredicate::Not(inner) => Ok(eval_predicate(inner, atoms)?.neg()),
        TriPredicate::And(terms) => {
            let mut terms = terms.iter();
            let Some(first) = terms.next() else {
                return Err(BatchError::EmptyTerms);
            };
            let mut accumulator = eval_predicate(first, atoms)?;
            for term in terms {
                let right = eval_predicate(term, atoms)?;
                accumulator = accumulator.meet(&right).map_err(map_vector_error)?;
            }
            Ok(accumulator)
        }
        TriPredicate::Or(terms) => {
            let mut terms = terms.iter();
            let Some(first) = terms.next() else {
                return Err(BatchError::EmptyTerms);
            };
            let mut accumulator = eval_predicate(first, atoms)?;
            for term in terms {
                let right = eval_predicate(term, atoms)?;
                accumulator = accumulator.join(&right).map_err(map_vector_error)?;
            }
            Ok(accumulator)
        }
    }
}

/// Indices where `truth` is `TRUE`.
///
/// This is SQL `WHERE` semantics: `FALSE` and `UNKNOWN` rows are dropped.
pub fn filter_indices(truth: &TritVec) -> Vec<usize> {
    truth
        .to_vec()
        .into_iter()
        .enumerate()
        .filter(|(_, value)| *value == K3::True)
        .map(|(index, _)| index)
        .collect()
}

/// For each left timestamp, the index of the last right row with `right <= left`.
///
/// `right` must be non-decreasing; the result has one entry per left row.
/// Equal timestamps resolve to the last matching right row (q's `aj`).
pub fn asof_join(left: &[i64], right: &[i64]) -> Result<Vec<Option<usize>>, BatchError> {
    for index in 1..right.len() {
        if right[index] < right[index - 1] {
            return Err(BatchError::UnorderedRight {
                index,
                previous: right[index - 1],
                current: right[index],
            });
        }
    }
    let mut out = Vec::with_capacity(left.len());
    for timestamp in left {
        let count = right.partition_point(|value| *value <= *timestamp);
        out.push(if count == 0 { None } else { Some(count - 1) });
    }
    Ok(out)
}

fn map_vector_error(error: TritVecError) -> BatchError {
    match error {
        TritVecError::LengthMismatch { expected, actual } => {
            BatchError::LengthMismatch { expected, actual }
        }
        other => BatchError::Vector(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tritdb_testkit::Rng;

    fn random_k3(rng: &mut Rng) -> K3 {
        match rng.below(3) {
            0 => K3::False,
            1 => K3::Unknown,
            _ => K3::True,
        }
    }

    fn random_atoms(rng: &mut Rng, rows: usize, atoms: usize) -> Vec<TritVec> {
        (0..atoms)
            .map(|_| {
                let values: Vec<K3> = (0..rows).map(|_| random_k3(rng)).collect();
                TritVec::new(&values)
            })
            .collect()
    }

    fn random_predicate(rng: &mut Rng, atoms: usize, depth: usize) -> TriPredicate {
        if depth == 0 || rng.chance(40) {
            return TriPredicate::Atom(rng.below(atoms as u32) as usize);
        }
        match rng.below(3) {
            0 => TriPredicate::Not(Box::new(random_predicate(rng, atoms, depth - 1))),
            1 => TriPredicate::And(vec![
                random_predicate(rng, atoms, depth - 1),
                random_predicate(rng, atoms, depth - 1),
            ]),
            _ => TriPredicate::Or(vec![
                random_predicate(rng, atoms, depth - 1),
                random_predicate(rng, atoms, depth - 1),
            ]),
        }
    }

    fn scalar_eval(predicate: &TriPredicate, row: &[K3]) -> K3 {
        match predicate {
            TriPredicate::Atom(index) => row[*index],
            TriPredicate::Not(inner) => scalar_eval(inner, row).neg(),
            TriPredicate::And(terms) => terms.iter().fold(K3::True, |accumulator, term| {
                accumulator.meet(scalar_eval(term, row))
            }),
            TriPredicate::Or(terms) => terms.iter().fold(K3::False, |accumulator, term| {
                accumulator.join(scalar_eval(term, row))
            }),
        }
    }

    #[test]
    fn batch_predicates_match_the_scalar_truth_tables() {
        let mut rng = Rng::new(0xB47C_0001);
        for _ in 0..300 {
            let rows = (rng.below(60) + 1) as usize;
            let atom_count = (rng.below(3) + 1) as usize;
            let atoms = random_atoms(&mut rng, rows, atom_count);
            let unpacked: Vec<Vec<K3>> = atoms.iter().map(TritVec::to_vec).collect();
            let predicate = random_predicate(&mut rng, atom_count, 3);
            let batch = eval_predicate(&predicate, &atoms)
                .expect("batch evaluation")
                .to_vec();
            assert_eq!(batch.len(), rows);
            for (row_index, actual) in batch.iter().enumerate() {
                let scalar_row: Vec<K3> = unpacked.iter().map(|column| column[row_index]).collect();
                assert_eq!(
                    *actual,
                    scalar_eval(&predicate, &scalar_row),
                    "row {row_index} of {predicate:?}"
                );
            }
        }
    }

    #[test]
    fn filter_keeps_only_true() {
        let truth = TritVec::new(&[K3::True, K3::False, K3::Unknown, K3::True]);
        assert_eq!(filter_indices(&truth), vec![0, 3]);
        assert!(filter_indices(&TritVec::new(&[])).is_empty());
    }

    #[test]
    fn batch_rejects_bad_inputs() {
        let short = TritVec::new(&[K3::True]);
        let long = TritVec::new(&[K3::True, K3::False]);
        assert_eq!(
            eval_predicate(&TriPredicate::Atom(3), std::slice::from_ref(&short)),
            Err(BatchError::AtomOutOfRange { index: 3, atoms: 1 })
        );
        assert_eq!(
            eval_predicate(
                &TriPredicate::And(vec![TriPredicate::Atom(0), TriPredicate::Atom(1)]),
                &[short, long],
            ),
            Err(BatchError::LengthMismatch {
                expected: 1,
                actual: 2
            })
        );
        assert_eq!(
            eval_predicate(&TriPredicate::And(vec![]), &[]),
            Err(BatchError::EmptyTerms)
        );
        assert_eq!(
            eval_predicate(&TriPredicate::Or(vec![]), &[]),
            Err(BatchError::EmptyTerms)
        );
    }

    fn asof_bruteforce(left: &[i64], right: &[i64]) -> Vec<Option<usize>> {
        left.iter()
            .map(|timestamp| {
                right
                    .iter()
                    .enumerate()
                    .filter(|(_, value)| *value <= timestamp)
                    .map(|(index, _)| index)
                    .next_back()
            })
            .collect()
    }

    #[test]
    fn asof_matches_bruteforce() {
        let mut rng = Rng::new(0xA50F_0002);
        for _ in 0..200 {
            let right_len = rng.below(20) as usize;
            let mut right: Vec<i64> = (0..right_len).map(|_| rng.below(100) as i64).collect();
            right.sort_unstable();
            let left: Vec<i64> = (0..rng.below(20) as usize)
                .map(|_| rng.below(100) as i64)
                .collect();
            assert_eq!(
                asof_join(&left, &right).unwrap(),
                asof_bruteforce(&left, &right),
                "left={left:?} right={right:?}"
            );
        }
    }

    #[test]
    fn asof_handles_edges() {
        assert_eq!(asof_join(&[], &[]).unwrap(), Vec::<Option<usize>>::new());
        assert_eq!(asof_join(&[1], &[]).unwrap(), vec![None]);
        assert_eq!(asof_join(&[5], &[1, 3, 5, 9]).unwrap(), vec![Some(2)]);
        // Equal timestamps: the last match wins.
        assert_eq!(asof_join(&[5], &[5, 5, 5]).unwrap(), vec![Some(2)]);
        // Unsorted right side is rejected.
        assert_eq!(
            asof_join(&[1], &[3, 2]),
            Err(BatchError::UnorderedRight {
                index: 1,
                previous: 3,
                current: 2
            })
        );
    }

    #[test]
    fn asof_scales_to_100k_rows() {
        let mut rng = Rng::new(0xA50F_0003);
        let mut right: Vec<i64> = (0..100_000).map(|_| rng.below(1_000_000) as i64).collect();
        right.sort_unstable();
        let left: Vec<i64> = (0..100_000).map(|_| rng.below(1_000_000) as i64).collect();
        let result = asof_join(&left, &right).expect("asof");
        assert_eq!(result.len(), left.len());
        let prefix = 200;
        assert_eq!(
            &result[..prefix],
            &asof_bruteforce(&left[..prefix], &right)[..]
        );
    }
}
