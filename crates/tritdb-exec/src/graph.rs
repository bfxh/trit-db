//! Annotated graphs: CSR adjacency, three-valued reachability, and annotated
//! k-nearest-neighbour queries.
//!
//! This is the in-memory half of M5. Callers build a graph from an edge list
//! and query it directly; persisting the CSR arrays into the page store is a
//! separate step that this module deliberately does not take.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

use crate::ann::RepairAnswer;
use crate::TruthValue;

/// Maximum nodes accepted by [`Csr::from_edges`].
pub const MAX_NODES: usize = 4096;
/// Maximum edges accepted by [`Csr::from_edges`].
pub const MAX_EDGES: usize = 16_384;
/// Maximum vector dimensions accepted by [`annotated_knn`].
pub const MAX_DIMENSIONS: usize = 64;

/// Structured graph errors.
#[derive(Debug)]
pub enum GraphError {
    /// Too many nodes for an in-memory CSR.
    TooManyNodes {
        /// Requested nodes.
        nodes: usize,
        /// Maximum supported.
        max: usize,
    },
    /// Too many edges after deduplication.
    TooManyEdges {
        /// Deduplicated edge count.
        edges: usize,
        /// Maximum supported.
        max: usize,
    },
    /// An edge endpoint is outside the declared node range.
    EdgeOutOfRange {
        /// Edge source.
        from: u32,
        /// Edge target.
        to: u32,
        /// Declared node count.
        nodes: usize,
    },
    /// A query node is outside the graph.
    NodeOutOfRange {
        /// Requested node.
        node: u32,
        /// Graph node count.
        nodes: usize,
    },
    /// A vector had no dimensions.
    EmptyVector,
    /// A vector had too many dimensions.
    TooManyDimensions {
        /// Requested dimensions.
        dimensions: usize,
        /// Maximum supported.
        max: usize,
    },
    /// A point and the query vector had different lengths.
    DimensionMismatch {
        /// Query length.
        expected: usize,
        /// Point length.
        actual: usize,
    },
    /// A squared distance did not fit in `i128`.
    DistanceOverflow,
}

impl fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::TooManyNodes { nodes, max } => {
                write!(formatter, "too many graph nodes: {nodes} > {max}")
            }
            GraphError::TooManyEdges { edges, max } => {
                write!(formatter, "too many graph edges: {edges} > {max}")
            }
            GraphError::EdgeOutOfRange { from, to, nodes } => write!(
                formatter,
                "edge {from}->{to} is outside 0..{nodes}; pass a larger --nodes"
            ),
            GraphError::NodeOutOfRange { node, nodes } => {
                write!(formatter, "node {node} is outside 0..{nodes}")
            }
            GraphError::EmptyVector => write!(formatter, "vector must have at least one dimension"),
            GraphError::TooManyDimensions { dimensions, max } => {
                write!(
                    formatter,
                    "too many vector dimensions: {dimensions} > {max}"
                )
            }
            GraphError::DimensionMismatch { expected, actual } => {
                write!(
                    formatter,
                    "point has {actual} dimensions, query has {expected}"
                )
            }
            GraphError::DistanceOverflow => {
                write!(formatter, "squared distance does not fit in i128")
            }
        }
    }
}

impl Error for GraphError {}

/// Compressed sparse row adjacency.
///
/// Every row is sorted and deduplicated, so [`Self::neighbors`] is stable and
/// [`Csr::has_edge`] can binary-search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Csr {
    offsets: Vec<usize>,
    targets: Vec<u32>,
}

impl Csr {
    /// Builds a CSR from a directed edge list.
    ///
    /// Duplicate edges are collapsed; `nodes` must cover every endpoint.
    pub fn from_edges(nodes: usize, edges: &[(u32, u32)]) -> Result<Self, GraphError> {
        if nodes > MAX_NODES {
            return Err(GraphError::TooManyNodes {
                nodes,
                max: MAX_NODES,
            });
        }
        let mut rows: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); nodes];
        for &(from, to) in edges {
            if from as usize >= nodes || to as usize >= nodes {
                return Err(GraphError::EdgeOutOfRange { from, to, nodes });
            }
            rows[from as usize].insert(to);
        }
        let edge_count: usize = rows.iter().map(BTreeSet::len).sum();
        if edge_count > MAX_EDGES {
            return Err(GraphError::TooManyEdges {
                edges: edge_count,
                max: MAX_EDGES,
            });
        }

        let mut offsets = Vec::with_capacity(nodes + 1);
        let mut targets = Vec::with_capacity(edge_count);
        offsets.push(0);
        for row in &rows {
            targets.extend(row.iter().copied());
            offsets.push(targets.len());
        }
        Ok(Self { offsets, targets })
    }

    /// Number of nodes, including nodes with no outgoing edges.
    pub fn node_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Number of deduplicated directed edges.
    pub fn edge_count(&self) -> usize {
        self.targets.len()
    }

    /// Outgoing neighbours of `node`, in ascending order.
    pub fn neighbors(&self, node: u32) -> Result<&[u32], GraphError> {
        self.check_node(node)?;
        Ok(self.row(node as usize))
    }

    /// Returns whether `from -> to` exists.
    pub fn has_edge(&self, from: u32, to: u32) -> Result<bool, GraphError> {
        self.check_node(from)?;
        self.check_node(to)?;
        Ok(self.row(from as usize).binary_search(&to).is_ok())
    }

    fn check_node(&self, node: u32) -> Result<(), GraphError> {
        if node as usize >= self.node_count() {
            return Err(GraphError::NodeOutOfRange {
                node,
                nodes: self.node_count(),
            });
        }
        Ok(())
    }

    fn row(&self, node: usize) -> &[u32] {
        &self.targets[self.offsets[node]..self.offsets[node + 1]]
    }
}

/// Result of a three-valued reachability query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach {
    /// `Certain` (all-true path), `Contingent` (only unknown edges) or
    /// `Impossible`.
    pub answer: RepairAnswer,
    /// Edge count of the witness path; `None` when impossible.
    pub hops: Option<usize>,
    /// Witness path, shortest first; empty when impossible.
    pub path: Vec<u32>,
}

impl Reach {
    fn certain(path: Vec<u32>) -> Self {
        Self {
            answer: RepairAnswer::Certain,
            hops: Some(path.len() - 1),
            path,
        }
    }

    fn contingent(path: Vec<u32>) -> Self {
        Self {
            answer: RepairAnswer::Contingent,
            hops: Some(path.len() - 1),
            path,
        }
    }

    fn impossible() -> Self {
        Self {
            answer: RepairAnswer::Impossible,
            hops: None,
            path: Vec::new(),
        }
    }
}

/// CSR adjacency plus one [`TruthValue`] label per edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnotatedGraph {
    csr: Csr,
    labels: Vec<TruthValue>,
}

impl AnnotatedGraph {
    /// Builds an annotated graph; a repeated edge keeps its last label.
    pub fn from_edges(nodes: usize, edges: &[(u32, u32, TruthValue)]) -> Result<Self, GraphError> {
        let pairs: Vec<(u32, u32)> = edges.iter().map(|&(from, to, _)| (from, to)).collect();
        let csr = Csr::from_edges(nodes, &pairs)?;
        let mut by_pair: BTreeMap<(u32, u32), TruthValue> = BTreeMap::new();
        for &(from, to, label) in edges {
            by_pair.insert((from, to), label);
        }
        let mut labels = Vec::with_capacity(csr.edge_count());
        for from in 0..csr.node_count() {
            for &to in csr.row(from) {
                let label = by_pair
                    .get(&(from as u32, to))
                    .copied()
                    .unwrap_or(TruthValue::Unknown);
                labels.push(label);
            }
        }
        Ok(Self { csr, labels })
    }

    /// Underlying CSR adjacency.
    pub fn graph(&self) -> &Csr {
        &self.csr
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.csr.node_count()
    }

    /// Number of deduplicated edges.
    pub fn edge_count(&self) -> usize {
        self.csr.edge_count()
    }

    /// Label of `from -> to`, or `None` when the edge is absent.
    pub fn edge_label(&self, from: u32, to: u32) -> Result<Option<TruthValue>, GraphError> {
        self.csr.check_node(from)?;
        self.csr.check_node(to)?;
        let row = self.csr.row(from as usize);
        Ok(row
            .binary_search(&to)
            .ok()
            .map(|index| self.labels[self.csr.offsets[from as usize] + index]))
    }

    /// Three-valued reachability from `from` to `to` within `max_hops` edges.
    ///
    /// A path only made of `TRUE` edges yields [`RepairAnswer::Certain`]; if no
    /// such path exists but one using `TRUE`/`UNKNOWN` edges does, the answer is
    /// [`RepairAnswer::Contingent`]; otherwise it is
    /// [`RepairAnswer::Impossible`]. `FALSE` edges are never traversable.
    pub fn reach(&self, from: u32, to: u32, max_hops: usize) -> Result<Reach, GraphError> {
        self.csr.check_node(from)?;
        self.csr.check_node(to)?;
        if let Some(path) = self.bfs_path(from, to, max_hops, false) {
            return Ok(Reach::certain(path));
        }
        if let Some(path) = self.bfs_path(from, to, max_hops, true) {
            return Ok(Reach::contingent(path));
        }
        Ok(Reach::impossible())
    }

    /// Nodes reachable from `start` within `max_hops`, breadth-first.
    ///
    /// Traverses `TRUE` and `UNKNOWN` edges; each node appears once with its
    /// shortest hop count.
    pub fn reachable_within(
        &self,
        start: u32,
        max_hops: usize,
    ) -> Result<Vec<(u32, usize)>, GraphError> {
        self.csr.check_node(start)?;
        let nodes = self.csr.node_count();
        let mut depth = vec![usize::MAX; nodes];
        let mut queue = VecDeque::new();
        let mut reached = Vec::new();
        depth[start as usize] = 0;
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            let hops = depth[node as usize];
            if hops >= max_hops {
                continue;
            }
            let base = self.csr.offsets[node as usize];
            for (offset, &next) in self.csr.row(node as usize).iter().enumerate() {
                if self.labels[base + offset] == TruthValue::False
                    || depth[next as usize] != usize::MAX
                {
                    continue;
                }
                depth[next as usize] = hops + 1;
                reached.push((next, hops + 1));
                queue.push_back(next);
            }
        }
        Ok(reached)
    }

    fn bfs_path(
        &self,
        start: u32,
        target: u32,
        max_hops: usize,
        allow_unknown: bool,
    ) -> Option<Vec<u32>> {
        if start == target {
            return Some(vec![start]);
        }
        let nodes = self.csr.node_count();
        let mut parent = vec![u32::MAX; nodes];
        let mut depth = vec![usize::MAX; nodes];
        let mut queue = VecDeque::new();
        depth[start as usize] = 0;
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            let hops = depth[node as usize];
            if hops >= max_hops {
                continue;
            }
            let base = self.csr.offsets[node as usize];
            for (offset, &next) in self.csr.row(node as usize).iter().enumerate() {
                let label = self.labels[base + offset];
                let allowed =
                    label == TruthValue::True || (allow_unknown && label == TruthValue::Unknown);
                if !allowed || depth[next as usize] != usize::MAX {
                    continue;
                }
                depth[next as usize] = hops + 1;
                parent[next as usize] = node;
                if next == target {
                    let mut path = vec![target];
                    let mut cursor = target;
                    while cursor != start {
                        cursor = parent[cursor as usize];
                        path.push(cursor);
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(next);
            }
        }
        None
    }
}

/// A labelled vector for [`annotated_knn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    /// Caller-defined identifier, echoed in results.
    pub id: u32,
    /// Integer coordinates.
    pub vector: Vec<i64>,
    /// Truth label: `True` = confirmed, `Unknown` = uncertain, `False` =
    /// excluded.
    pub label: TruthValue,
}

/// One scored point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Neighbor {
    /// Point identifier.
    pub id: u32,
    /// Squared Euclidean distance (exact integer arithmetic).
    pub distance: i128,
    /// Point label.
    pub label: TruthValue,
}

/// Top-k neighbours split by three-valued label.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnNeighbors {
    /// Top-k over `True` points.
    pub certain: Vec<Neighbor>,
    /// Top-k over `Unknown` points.
    pub uncertain: Vec<Neighbor>,
    /// Top-k over `False` points, kept for diagnostics.
    pub excluded: Vec<Neighbor>,
}

/// Three-valued k-nearest-neighbour query over labelled integer vectors.
///
/// Distance is exact squared Euclidean (`i128`), so results are deterministic.
/// Each bucket holds at most `k` neighbours, ordered by distance then `id`.
pub fn annotated_knn(
    points: &[Point],
    query: &[i64],
    k: usize,
) -> Result<AnnNeighbors, GraphError> {
    if query.is_empty() {
        return Err(GraphError::EmptyVector);
    }
    if query.len() > MAX_DIMENSIONS {
        return Err(GraphError::TooManyDimensions {
            dimensions: query.len(),
            max: MAX_DIMENSIONS,
        });
    }
    let mut scored = Vec::with_capacity(points.len());
    for point in points {
        if point.vector.len() != query.len() {
            return Err(GraphError::DimensionMismatch {
                expected: query.len(),
                actual: point.vector.len(),
            });
        }
        let mut distance: i128 = 0;
        for (coordinate, wanted) in point.vector.iter().zip(query) {
            let difference = i128::from(*coordinate) - i128::from(*wanted);
            let square = difference
                .checked_mul(difference)
                .ok_or(GraphError::DistanceOverflow)?;
            distance = distance
                .checked_add(square)
                .ok_or(GraphError::DistanceOverflow)?;
        }
        scored.push(Neighbor {
            id: point.id,
            distance,
            label: point.label,
        });
    }
    scored.sort_by(|left, right| {
        left.distance
            .cmp(&right.distance)
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut answer = AnnNeighbors::default();
    for neighbor in scored {
        let bucket = match neighbor.label {
            TruthValue::True => &mut answer.certain,
            TruthValue::Unknown => &mut answer.uncertain,
            TruthValue::False => &mut answer.excluded,
        };
        if bucket.len() < k {
            bucket.push(neighbor);
        }
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(edges: &[(u32, u32, TruthValue)]) -> AnnotatedGraph {
        let nodes = edges
            .iter()
            .map(|&(from, to, _)| from.max(to) as usize + 1)
            .max()
            .unwrap_or(0);
        AnnotatedGraph::from_edges(nodes, edges).expect("build graph")
    }

    #[test]
    fn csr_sorts_dedups_and_rejects_out_of_range_edges() {
        let csr = Csr::from_edges(3, &[(1, 2), (0, 2), (0, 1), (0, 1)]).expect("csr");
        assert_eq!(csr.node_count(), 3);
        assert_eq!(csr.edge_count(), 3);
        assert_eq!(csr.neighbors(0).expect("row 0"), &[1, 2]);
        assert_eq!(csr.neighbors(2).expect("row 2"), &[] as &[u32]);
        assert!(csr.has_edge(0, 2).expect("has edge"));
        assert!(!csr.has_edge(2, 0).expect("has edge"));

        let error = Csr::from_edges(2, &[(0, 2)]).expect_err("out of range");
        assert!(matches!(error, GraphError::EdgeOutOfRange { .. }));
        let error = csr.neighbors(3).expect_err("node out of range");
        assert!(matches!(error, GraphError::NodeOutOfRange { .. }));
    }

    #[test]
    fn true_path_reaches_certainly() {
        let graph = build(&[(0, 1, TruthValue::True), (1, 2, TruthValue::True)]);
        let reach = graph.reach(0, 2, 2).expect("reach");
        assert_eq!(reach.answer, RepairAnswer::Certain);
        assert_eq!(reach.hops, Some(2));
        assert_eq!(reach.path, vec![0, 1, 2]);
    }

    #[test]
    fn unknown_edge_makes_reach_contingent() {
        let graph = build(&[(0, 1, TruthValue::True), (1, 2, TruthValue::Unknown)]);
        let reach = graph.reach(0, 2, 2).expect("reach");
        assert_eq!(reach.answer, RepairAnswer::Contingent);
        assert_eq!(reach.path, vec![0, 1, 2]);

        // A definite shortcut wins over the uncertain path.
        let graph = build(&[
            (0, 1, TruthValue::True),
            (1, 2, TruthValue::Unknown),
            (0, 2, TruthValue::True),
        ]);
        let reach = graph.reach(0, 2, 2).expect("reach");
        assert_eq!(reach.answer, RepairAnswer::Certain);
        assert_eq!(reach.path, vec![0, 2]);
    }

    #[test]
    fn false_edge_blocks_reach() {
        let graph = build(&[(0, 1, TruthValue::True), (1, 2, TruthValue::False)]);
        let reach = graph.reach(0, 2, 2).expect("reach");
        assert_eq!(reach.answer, RepairAnswer::Impossible);
        assert_eq!(reach.hops, None);
        assert!(reach.path.is_empty());
    }

    #[test]
    fn max_hops_bounds_reach() {
        let graph = build(&[(0, 1, TruthValue::True), (1, 2, TruthValue::True)]);
        assert_eq!(
            graph.reach(0, 2, 1).expect("reach").answer,
            RepairAnswer::Impossible
        );
        let reach = graph.reach(0, 1, 1).expect("reach");
        assert_eq!(reach.answer, RepairAnswer::Certain);
        // A zero-hop query only succeeds from a node to itself.
        assert_eq!(graph.reach(0, 0, 0).expect("reach").path, vec![0]);
    }

    #[test]
    fn reachable_within_returns_breadth_first_layers() {
        let graph = build(&[
            (0, 1, TruthValue::True),
            (0, 2, TruthValue::Unknown),
            (1, 3, TruthValue::True),
            (2, 4, TruthValue::False),
        ]);
        let reached = graph.reachable_within(0, 2).expect("reached");
        assert_eq!(reached, vec![(1, 1), (2, 1), (3, 2)]);
        let one_hop = graph.reachable_within(0, 1).expect("one hop");
        assert_eq!(one_hop, vec![(1, 1), (2, 1)]);
    }

    #[test]
    fn annotated_knn_groups_by_label_and_breaks_ties() {
        let points = vec![
            Point {
                id: 0,
                vector: vec![0, 0],
                label: TruthValue::True,
            },
            Point {
                id: 1,
                vector: vec![1, 0],
                label: TruthValue::Unknown,
            },
            Point {
                id: 2,
                vector: vec![2, 0],
                label: TruthValue::False,
            },
            Point {
                id: 3,
                vector: vec![0, 1],
                label: TruthValue::Unknown,
            },
        ];
        let answer = annotated_knn(&points, &[0, 0], 2).expect("knn");
        assert_eq!(answer.certain.len(), 1);
        assert_eq!(answer.certain[0].id, 0);
        assert_eq!(answer.certain[0].distance, 0);
        assert_eq!(
            answer.uncertain.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            answer.excluded.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn annotated_knn_rejects_dimension_mismatch_and_overflow() {
        let points = vec![Point {
            id: 0,
            vector: vec![1],
            label: TruthValue::True,
        }];
        let error = annotated_knn(&points, &[1, 2], 1).expect_err("dimensions");
        assert!(matches!(error, GraphError::DimensionMismatch { .. }));
        let error = annotated_knn(&[], &[], 1).expect_err("empty query");
        assert!(matches!(error, GraphError::EmptyVector));

        let huge = vec![Point {
            id: 0,
            vector: vec![i64::MAX],
            label: TruthValue::True,
        }];
        let error = annotated_knn(&huge, &[i64::MIN], 1).expect_err("overflow");
        assert!(matches!(error, GraphError::DistanceOverflow));
    }
}
