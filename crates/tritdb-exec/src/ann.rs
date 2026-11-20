//! Annotated relations: K3 / Belnap / Łukasiewicz / ℕ annotations, exact
//! model counting, and consistent query answering over minimal repairs.
//!
//! This is the semantic core of M3. It deliberately does not touch the SQL
//! parser yet: callers build [`BoolExpr`] or fact sets directly.

use std::fmt;

use crate::TruthValue;

/// Maximum variables accepted by [`count_models`].
pub const MAX_MODEL_VARS: usize = 20;
/// Maximum facts accepted by [`under_repair`].
pub const MAX_REPAIR_FACTS: usize = 64;
/// Maximum facts inside one connected component of the denial graph.
pub const MAX_COMPONENT_FACTS: usize = 16;

/// Belnap four-valued truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum B4 {
    /// True in every model.
    True,
    /// False in every model.
    False,
    /// Both true and false (conflict).
    Both,
    /// Neither true nor false (no information).
    Neither,
}

impl B4 {
    /// Belnap negation: `True <-> False`, `Both` and `Neither` are fixed.
    #[allow(clippy::should_implement_trait)]
    pub const fn neg(self) -> Self {
        match self {
            B4::True => B4::False,
            B4::False => B4::True,
            B4::Both => B4::Both,
            B4::Neither => B4::Neither,
        }
    }

    /// Conjunction (meet in the truth lattice).
    pub const fn meet(self, other: Self) -> Self {
        match (self, other) {
            (B4::False, _) | (_, B4::False) => B4::False,
            (B4::True, value) | (value, B4::True) => value,
            (B4::Both, B4::Both) => B4::Both,
            (B4::Neither, B4::Neither) => B4::Neither,
            _ => B4::False,
        }
    }

    /// Disjunction (join in the truth lattice).
    pub const fn join(self, other: Self) -> Self {
        match (self, other) {
            (B4::True, _) | (_, B4::True) => B4::True,
            (B4::False, value) | (value, B4::False) => value,
            (B4::Both, B4::Both) => B4::Both,
            (B4::Neither, B4::Neither) => B4::Neither,
            _ => B4::True,
        }
    }
}

/// An annotation from one of four semirings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Annotation {
    /// Kleene three-valued logic (same truth table as SQL `NULL`).
    K3(TruthValue),
    /// Belnap four-valued logic.
    Belnap(B4),
    /// Łukasiewicz `[0, 1]` chain.
    Lukasiewicz(f64),
    /// Natural numbers as a counting semiring.
    Nat(u64),
}

impl Annotation {
    /// Builds a Łukasiewicz annotation, rejecting values outside `[0, 1]`.
    pub fn lukasiewicz(value: f64) -> Result<Self, AnnError> {
        if (0.0..=1.0).contains(&value) {
            Ok(Annotation::Lukasiewicz(value))
        } else {
            Err(AnnError::LukasiewiczOutOfRange)
        }
    }

    /// Negation. `Nat` has no negation in the counting semiring.
    #[allow(clippy::should_implement_trait)]
    pub fn neg(self) -> Result<Self, AnnError> {
        match self {
            Annotation::K3(value) => Ok(Annotation::K3(value.not())),
            Annotation::Belnap(value) => Ok(Annotation::Belnap(value.neg())),
            Annotation::Lukasiewicz(value) => Ok(Annotation::Lukasiewicz(1.0 - value)),
            Annotation::Nat(_) => Err(AnnError::NegationUndefined("Nat")),
        }
    }

    /// Conjunction. K3/Belnap use their truth tables; Łukasiewicz uses `min`;
    /// `Nat` uses multiplication (independent-event counting).
    pub fn meet(self, other: Self) -> Result<Self, AnnError> {
        match (self, other) {
            (Annotation::K3(left), Annotation::K3(right)) => Ok(Annotation::K3(left.and(right))),
            (Annotation::Belnap(left), Annotation::Belnap(right)) => {
                Ok(Annotation::Belnap(left.meet(right)))
            }
            (Annotation::Lukasiewicz(left), Annotation::Lukasiewicz(right)) => {
                Ok(Annotation::Lukasiewicz(left.min(right)))
            }
            (Annotation::Nat(left), Annotation::Nat(right)) => {
                Ok(Annotation::Nat(left.saturating_mul(right)))
            }
            _ => Err(AnnError::DomainMismatch),
        }
    }

    /// Disjunction. K3/Belnap use their truth tables; Łukasiewicz uses `max`;
    /// `Nat` uses addition (union counting).
    pub fn join(self, other: Self) -> Result<Self, AnnError> {
        match (self, other) {
            (Annotation::K3(left), Annotation::K3(right)) => Ok(Annotation::K3(left.or(right))),
            (Annotation::Belnap(left), Annotation::Belnap(right)) => {
                Ok(Annotation::Belnap(left.join(right)))
            }
            (Annotation::Lukasiewicz(left), Annotation::Lukasiewicz(right)) => {
                Ok(Annotation::Lukasiewicz(left.max(right)))
            }
            (Annotation::Nat(left), Annotation::Nat(right)) => {
                Ok(Annotation::Nat(left.saturating_add(right)))
            }
            _ => Err(AnnError::DomainMismatch),
        }
    }
}

/// Errors from annotation evaluation and counting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnnError {
    /// Operations mixed two annotation domains.
    DomainMismatch,
    /// A Łukasiewicz value was outside `[0, 1]`.
    LukasiewiczOutOfRange,
    /// The domain has no negation.
    NegationUndefined(&'static str),
    /// Too many variables for exact enumeration.
    TooManyVars {
        /// Requested variables.
        vars: usize,
        /// Maximum supported.
        max: usize,
    },
    /// Too many facts for exact repair enumeration.
    TooManyFacts {
        /// Requested facts.
        facts: usize,
        /// Maximum supported.
        max: usize,
    },
    /// A variable or fact index was outside the declared range.
    OutOfRange {
        /// Requested index.
        index: usize,
        /// Declared count.
        count: usize,
    },
    /// A boolean expression could not be parsed.
    Parse(String),
}

impl fmt::Display for AnnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnnError::DomainMismatch => write!(formatter, "annotation domains do not match"),
            AnnError::LukasiewiczOutOfRange => {
                write!(formatter, "Lukasiewicz annotation must be in [0, 1]")
            }
            AnnError::NegationUndefined(domain) => {
                write!(formatter, "{domain} annotations have no negation")
            }
            AnnError::TooManyVars { vars, max } => {
                write!(formatter, "too many variables: {vars} > {max}")
            }
            AnnError::TooManyFacts { facts, max } => {
                write!(formatter, "too many facts: {facts} > {max}")
            }
            AnnError::OutOfRange { index, count } => {
                write!(formatter, "index {index} is outside 0..{count}")
            }
            AnnError::Parse(reason) => write!(formatter, "bad boolean expression: {reason}"),
        }
    }
}

impl std::error::Error for AnnError {}

/// Boolean expression used by [`count_models`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoolExpr {
    /// Variable by index.
    Var(usize),
    /// Constant.
    Const(bool),
    /// Negation.
    Not(Box<BoolExpr>),
    /// Conjunction.
    And(Vec<BoolExpr>),
    /// Disjunction.
    Or(Vec<BoolExpr>),
}

impl BoolExpr {
    /// Evaluates under `assignment[i]` for `Var(i)`.
    pub fn eval(&self, assignment: &[bool]) -> Result<bool, AnnError> {
        match self {
            BoolExpr::Const(value) => Ok(*value),
            BoolExpr::Var(index) => assignment.get(*index).copied().ok_or(AnnError::OutOfRange {
                index: *index,
                count: assignment.len(),
            }),
            BoolExpr::Not(inner) => Ok(!inner.eval(assignment)?),
            BoolExpr::And(items) => {
                for item in items {
                    if !item.eval(assignment)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            BoolExpr::Or(items) => {
                for item in items {
                    if item.eval(assignment)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }
}

/// Counts satisfying assignments exactly by enumeration.
///
/// `vars` must not exceed [`MAX_MODEL_VARS`]; this is an exact counter, not a
/// sampler.
pub fn count_models(expr: &BoolExpr, vars: usize) -> Result<u64, AnnError> {
    if vars > MAX_MODEL_VARS {
        return Err(AnnError::TooManyVars {
            vars,
            max: MAX_MODEL_VARS,
        });
    }
    let mut assignment = vec![false; vars];
    let mut models = 0u64;
    for mask in 0..(1u64 << vars) {
        for (index, value) in assignment.iter_mut().enumerate() {
            *value = mask & (1 << index) != 0;
        }
        if expr.eval(&assignment)? {
            models += 1;
        }
    }
    Ok(models)
}

/// Answer of a query under all minimal repairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairAnswer {
    /// True in every minimal repair.
    Certain,
    /// False in every minimal repair.
    Impossible,
    /// True in some repairs and false in others (Belnap `Both`).
    Contingent,
}

/// Consistent query answering over minimal repairs.
///
/// `denials` are sets of facts that must not all hold; a repair is a maximal
/// consistent subset of the facts. `query` is a set of facts that must all
/// hold.
pub fn under_repair(
    facts: usize,
    denials: &[Vec<usize>],
    query: &[usize],
) -> Result<RepairAnswer, AnnError> {
    if facts > MAX_REPAIR_FACTS {
        return Err(AnnError::TooManyFacts {
            facts,
            max: MAX_REPAIR_FACTS,
        });
    }
    for denial in denials {
        for index in denial {
            if *index >= facts {
                return Err(AnnError::OutOfRange {
                    index: *index,
                    count: facts,
                });
            }
        }
    }
    for index in query {
        if *index >= facts {
            return Err(AnnError::OutOfRange {
                index: *index,
                count: facts,
            });
        }
    }

    // Denials only connect facts inside one component. Answers compose
    // independently, so each component is enumerated on its own.
    let mut any_impossible = false;
    let mut any_contingent = false;
    for component in components(facts, denials) {
        let position: std::collections::BTreeMap<usize, usize> = component
            .iter()
            .enumerate()
            .map(|(local, &fact)| (fact, local))
            .collect();
        let local_denials: Vec<Vec<usize>> = denials
            .iter()
            .filter(|denial| {
                denial
                    .first()
                    .is_some_and(|first| position.contains_key(first))
            })
            .map(|denial| denial.iter().map(|fact| position[fact]).collect())
            .collect();
        let local_query: Vec<usize> = query
            .iter()
            .filter_map(|fact| position.get(fact).copied())
            .collect();
        match component_answer(component.len(), &local_denials, &local_query)? {
            RepairAnswer::Impossible => any_impossible = true,
            RepairAnswer::Contingent => any_contingent = true,
            RepairAnswer::Certain => {}
        }
    }

    Ok(if any_impossible {
        RepairAnswer::Impossible
    } else if any_contingent {
        RepairAnswer::Contingent
    } else {
        RepairAnswer::Certain
    })
}

fn component_answer(
    facts: usize,
    denials: &[Vec<usize>],
    query: &[usize],
) -> Result<RepairAnswer, AnnError> {
    if facts > MAX_COMPONENT_FACTS {
        return Err(AnnError::TooManyFacts {
            facts,
            max: MAX_COMPONENT_FACTS,
        });
    }
    let mut certain = true;
    let mut possible = false;
    for mask in 0..(1u64 << facts) {
        if !consistent(mask, denials) || !maximal(mask, facts, denials) {
            continue;
        }
        let holds = query.iter().all(|index| mask & (1 << index) != 0);
        if holds {
            possible = true;
        } else {
            certain = false;
        }
    }
    Ok(if certain {
        RepairAnswer::Certain
    } else if possible {
        RepairAnswer::Contingent
    } else {
        RepairAnswer::Impossible
    })
}

fn components(facts: usize, denials: &[Vec<usize>]) -> Vec<Vec<usize>> {
    fn find(parent: &mut [usize], mut value: usize) -> usize {
        while parent[value] != value {
            parent[value] = parent[parent[value]];
            value = parent[value];
        }
        value
    }

    let mut parent: Vec<usize> = (0..facts).collect();
    for denial in denials {
        if let Some(&first) = denial.first() {
            for &other in &denial[1..] {
                let left = find(&mut parent, first);
                let right = find(&mut parent, other);
                if left != right {
                    parent[left] = right;
                }
            }
        }
    }

    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for fact in 0..facts {
        let root = find(&mut parent, fact);
        groups.entry(root).or_default().push(fact);
    }
    groups.into_values().collect()
}

fn consistent(mask: u64, denials: &[Vec<usize>]) -> bool {
    denials
        .iter()
        .all(|denial| !denial.iter().all(|index| mask & (1 << index) != 0))
}

fn maximal(mask: u64, facts: usize, denials: &[Vec<usize>]) -> bool {
    (0..facts).all(|index| mask & (1 << index) != 0 || !consistent(mask | (1 << index), denials))
}

/// Parses a boolean expression over `a`..`z`, `vN`, or `N` variables.
///
/// Returns the expression and the minimal variable count (maximum index + 1).
pub fn parse_bool_expr(text: &str) -> Result<(BoolExpr, usize), AnnError> {
    let expr = BoolParser::new(text).parse()?;
    let mut max_index = None;
    collect_vars(&expr, &mut max_index);
    let vars = max_index.map_or(0, |index| index + 1);
    if vars > MAX_MODEL_VARS {
        return Err(AnnError::TooManyVars {
            vars,
            max: MAX_MODEL_VARS,
        });
    }
    Ok((expr, vars))
}

fn collect_vars(expr: &BoolExpr, max_index: &mut Option<usize>) {
    match expr {
        BoolExpr::Var(index) => {
            *max_index = Some(max_index.map_or(*index, |current| current.max(*index)));
        }
        BoolExpr::Const(_) => {}
        BoolExpr::Not(inner) => collect_vars(inner, max_index),
        BoolExpr::And(items) | BoolExpr::Or(items) => {
            for item in items {
                collect_vars(item, max_index);
            }
        }
    }
}

struct BoolParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> BoolParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
        }
    }

    fn parse(mut self) -> Result<BoolExpr, AnnError> {
        let expr = self.parse_or()?;
        self.skip_ws();
        if self.position < self.input.len() {
            return Err(AnnError::Parse(format!(
                "unexpected input at byte {}",
                self.position
            )));
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> Result<BoolExpr, AnnError> {
        let mut items = vec![self.parse_and()?];
        loop {
            self.skip_ws();
            if self.eat_word("OR") || self.eat("||") {
                items.push(self.parse_and()?);
            } else {
                break;
            }
        }
        if items.len() == 1 {
            Ok(items.pop().expect("one item"))
        } else {
            Ok(BoolExpr::Or(items))
        }
    }

    fn parse_and(&mut self) -> Result<BoolExpr, AnnError> {
        let mut items = vec![self.parse_not()?];
        loop {
            self.skip_ws();
            if self.eat_word("AND") || self.eat("&&") {
                items.push(self.parse_not()?);
            } else {
                break;
            }
        }
        if items.len() == 1 {
            Ok(items.pop().expect("one item"))
        } else {
            Ok(BoolExpr::And(items))
        }
    }

    fn parse_not(&mut self) -> Result<BoolExpr, AnnError> {
        self.skip_ws();
        if self.eat_word("NOT") || self.eat("!") {
            return Ok(BoolExpr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<BoolExpr, AnnError> {
        self.skip_ws();
        if self.eat("(") {
            let expr = self.parse_or()?;
            self.skip_ws();
            if !self.eat(")") {
                return Err(AnnError::Parse("missing ')'".to_string()));
            }
            return Ok(expr);
        }
        if self.eat_word("TRUE") {
            return Ok(BoolExpr::Const(true));
        }
        if self.eat_word("FALSE") {
            return Ok(BoolExpr::Const(false));
        }
        let name = self.ident()?;
        Ok(BoolExpr::Var(parse_var_name(&name)?))
    }

    fn ident(&mut self) -> Result<String, AnnError> {
        self.skip_ws();
        let start = self.position;
        while self.position < self.input.len()
            && (self.input[self.position].is_ascii_alphanumeric()
                || self.input[self.position] == b'_')
        {
            self.position += 1;
        }
        if start == self.position {
            return Err(AnnError::Parse(format!(
                "expected a variable at byte {start}"
            )));
        }
        Ok(String::from_utf8_lossy(&self.input[start..self.position]).into_owned())
    }

    fn eat(&mut self, token: &str) -> bool {
        let bytes = token.as_bytes();
        if self.input[self.position..].starts_with(bytes) {
            self.position += bytes.len();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        self.skip_ws();
        let bytes = word.as_bytes();
        if !self.input[self.position..].starts_with(bytes) {
            return false;
        }
        let end = self.position + bytes.len();
        if end < self.input.len()
            && (self.input[end].is_ascii_alphanumeric() || self.input[end] == b'_')
        {
            return false;
        }
        self.position = end;
        true
    }

    fn skip_ws(&mut self) {
        while self.position < self.input.len() && self.input[self.position].is_ascii_whitespace() {
            self.position += 1;
        }
    }
}

fn parse_var_name(name: &str) -> Result<usize, AnnError> {
    if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
        return Ok(usize::from(name.as_bytes()[0] - b'a'));
    }
    if let Some(rest) = name.strip_prefix('v') {
        return rest
            .parse::<usize>()
            .map_err(|_| AnnError::Parse(format!("invalid variable: {name}")));
    }
    name.parse::<usize>()
        .map_err(|_| AnnError::Parse(format!("invalid variable: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(index: usize) -> BoolExpr {
        BoolExpr::Var(index)
    }

    #[test]
    fn belnap_truth_tables_are_lattice_operations() {
        assert_eq!(B4::True.meet(B4::Both), B4::Both);
        assert_eq!(B4::False.meet(B4::Both), B4::False);
        assert_eq!(B4::Both.meet(B4::Neither), B4::False);
        assert_eq!(B4::True.join(B4::Both), B4::True);
        assert_eq!(B4::False.join(B4::Both), B4::Both);
        assert_eq!(B4::Both.join(B4::Neither), B4::True);
        assert_eq!(B4::Both.neg(), B4::Both);
        assert_eq!(B4::Neither.neg(), B4::Neither);
    }

    #[test]
    fn annotation_operations_respect_domains() {
        let k3 = Annotation::K3(TruthValue::Unknown);
        assert_eq!(
            k3.meet(Annotation::K3(TruthValue::True)).unwrap(),
            Annotation::K3(TruthValue::Unknown)
        );
        assert_eq!(
            Annotation::Belnap(B4::Both)
                .join(Annotation::Belnap(B4::False))
                .unwrap(),
            Annotation::Belnap(B4::Both)
        );
        assert_eq!(
            Annotation::Nat(3).meet(Annotation::Nat(4)).unwrap(),
            Annotation::Nat(12)
        );
        assert_eq!(
            Annotation::Nat(3).join(Annotation::Nat(4)).unwrap(),
            Annotation::Nat(7)
        );
        assert_eq!(
            Annotation::lukasiewicz(0.25).unwrap().neg().unwrap(),
            Annotation::Lukasiewicz(0.75)
        );
        assert_eq!(k3.meet(Annotation::Nat(1)), Err(AnnError::DomainMismatch));
        assert_eq!(
            Annotation::Nat(1).neg(),
            Err(AnnError::NegationUndefined("Nat"))
        );
        assert_eq!(
            Annotation::lukasiewicz(1.5),
            Err(AnnError::LukasiewiczOutOfRange)
        );
    }

    #[test]
    fn count_models_enumerates_exactly() {
        // a AND (NOT b OR c)
        let expr = BoolExpr::And(vec![
            var(0),
            BoolExpr::Or(vec![BoolExpr::Not(Box::new(var(1))), var(2)]),
        ]);
        assert_eq!(count_models(&expr, 3).unwrap(), 3);

        // XOR over two variables has two models.
        let xor = BoolExpr::And(vec![
            BoolExpr::Or(vec![var(0), var(1)]),
            BoolExpr::Not(Box::new(BoolExpr::And(vec![var(0), var(1)]))),
        ]);
        assert_eq!(count_models(&xor, 2).unwrap(), 2);

        // A free variable doubles the model count.
        assert_eq!(count_models(&BoolExpr::Const(true), 3).unwrap(), 8);
        assert_eq!(
            count_models(&BoolExpr::Const(true), MAX_MODEL_VARS + 1),
            Err(AnnError::TooManyVars {
                vars: MAX_MODEL_VARS + 1,
                max: MAX_MODEL_VARS
            })
        );
    }

    #[test]
    fn repairs_answer_certain_possible_and_impossible() {
        // Two conflicting facts: one of them survives in each repair.
        assert_eq!(
            under_repair(2, &[vec![0, 1]], &[0]).unwrap(),
            RepairAnswer::Contingent
        );
        assert_eq!(
            under_repair(2, &[vec![0, 1]], &[0, 1]).unwrap(),
            RepairAnswer::Impossible
        );

        // No constraints: every fact is certain.
        assert_eq!(under_repair(2, &[], &[0]).unwrap(), RepairAnswer::Certain);

        // Chain conflict a-b, b-c: repairs are {a,c} and {b}.
        assert_eq!(
            under_repair(3, &[vec![0, 1], vec![1, 2]], &[0, 2]).unwrap(),
            RepairAnswer::Contingent
        );
        assert_eq!(
            under_repair(3, &[vec![0, 1], vec![1, 2]], &[1]).unwrap(),
            RepairAnswer::Contingent
        );
        assert_eq!(
            under_repair(3, &[vec![0, 1], vec![1, 2]], &[0, 1]).unwrap(),
            RepairAnswer::Impossible
        );
    }

    #[test]
    fn repairs_decompose_into_connected_components() {
        // 20 facts, two independent conflicts. Only the first component is
        // contested; the second one is irrelevant to these queries.
        let denials = vec![vec![0, 1], vec![10, 11]];
        assert_eq!(
            under_repair(20, &denials, &[0]).unwrap(),
            RepairAnswer::Contingent
        );
        assert_eq!(
            under_repair(20, &denials, &[2]).unwrap(),
            RepairAnswer::Certain
        );
        assert_eq!(
            under_repair(20, &denials, &[0, 1]).unwrap(),
            RepairAnswer::Impossible
        );
    }

    #[test]
    fn repairs_reject_an_oversized_component() {
        let facts = MAX_COMPONENT_FACTS + 1;
        let denials: Vec<Vec<usize>> = (0..facts - 1).map(|i| vec![i, i + 1]).collect();
        assert_eq!(
            under_repair(facts, &denials, &[0]),
            Err(AnnError::TooManyFacts {
                facts,
                max: MAX_COMPONENT_FACTS
            })
        );
    }

    #[test]
    fn repairs_reject_out_of_range_inputs() {
        assert_eq!(
            under_repair(2, &[vec![0, 5]], &[0]),
            Err(AnnError::OutOfRange { index: 5, count: 2 })
        );
        assert_eq!(
            under_repair(MAX_REPAIR_FACTS + 1, &[], &[]),
            Err(AnnError::TooManyFacts {
                facts: MAX_REPAIR_FACTS + 1,
                max: MAX_REPAIR_FACTS
            })
        );
    }
}
