//! Solver (Data › Analysis › Solver): finds values for variable cells that maximise, minimise
//! or hit a value in an objective cell, subject to constraints.
//!
//! The workbook is the model: every evaluation sets the variable cells on a copy of the
//! workbook and recalculates their dependents ([`Trial`]), so any formula works. Three methods:
//!
//! - **Simplex LP**: the model is linearised from `n + 1` evaluations and checked for linearity
//!   at one more point; then a dense two-phase simplex (Dantzig's rule, Bland's rule when it
//!   stalls) solves it exactly. Integer and binary constraints use depth-first branch and bound.
//! - **GRG Nonlinear** (the name Excel's dialog uses): GridCraft's method is *not* a generalised
//!   reduced gradient. It is an augmented-Lagrangian method (Powell–Hestenes–Rockafellar) whose
//!   inner problems are solved by projected gradient descent with Barzilai–Borwein steps and an
//!   Armijo line search, on central- or forward-difference gradients, scaled by the variables'
//!   sizes (Use Automatic Scaling). Variable bounds are kept by projection. It finds a local
//!   optimum of a smooth model; integer constraints use a small branch and bound on top.
//!   Multistart runs it from random points in the bounds and keeps the best.
//!
//! - **Evolutionary**: a genetic search for models that aren't smooth (IF, lookups, rounding),
//!   with tournament selection, blend crossover and Gaussian mutation inside the variable bounds,
//!   feasibility-first ranking and integer rounding, followed by a compass (pattern) search around
//!   the best member. It stops when the population has converged, when the best member stops
//!   improving, or at the evaluation budget.
//!
//! Work is bounded by an evaluation budget (no clock: `Instant` traps on wasm). The model and its
//! Solver Options are saved as hidden sheet-scoped names (`solver_opt`, `solver_adj`,
//! `solver_lhs1`, `solver_pre`…), so they travel with the workbook through XLSX.
//!
//! Reports (`data.solverReport`, or `reports` on a solve) go on new sheets: Answer (the result),
//! Sensitivity (Simplex LP: reduced costs, shadow prices and their ranges from the final simplex
//! tableau; GRG: reduced gradients and Lagrange multipliers at the local optimum) and Limits.

use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, DefinedName, Sheet, Workbook};
use serde_json::{Value as Json, json};

use super::whatif::Trial;
use super::*;

const CMD: &str = "data.solver";
const REPORT_CMD: &str = "data.solverReport";
/// Excel's limit on decision variables.
const MAX_VARS: usize = 200;
/// Most scalar constraints (after expanding ranges).
const MAX_CONSTRAINTS: usize = 1000;
const DEFAULT_EVALS: u64 = 20_000;
const MAX_EVALS: u64 = 200_000;
/// Most simplex pivots × tableau cells per solve.
const LP_WORK: u64 = 2_000_000_000;
const MAX_NODES_LP: usize = 10_000;
const MAX_NODES_NLP: usize = 200;
/// Evolutionary population sizes.
const MIN_POPULATION: usize = 10;
const MAX_POPULATION: usize = 1000;
/// Constraint precision (Excel's default 0.000001).
const PRECISION: f64 = 1e-6;
const EPS: f64 = 1e-9;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "data.solver",
            "Solver…",
            ["Data", "Analysis"],
            None,
            "{objective: \"B10\" (formula cell), goal?: max|min|value (default max), value?: number (for goal value), variables: \"B2:B4,D1\" (≤200 constant cells, same sheet), constraints?: [\"B5 <= 10\", \"B2:B4 >= C2:C4\", \"B2 int\", \"B3 bin\", {lhs: \"B6\", op: \"=\", rhs: 3 | \"C6\"}], nonNegative?: bool (default true: variables without a lower bound get 0), method?: grg|simplex|evolutionary (default grg), populationSize?: 100, mutationRate?: 0.075, randomSeed?: 0 (a fixed seed, so runs repeat), convergence?: 0.0001, generationsWithoutImprovement?: 50, requireBounds?: bool (default true: every variable needs both bounds; false searches within ±100 of the start) (Evolutionary and Multistart options), Solver Options: precision?: 0.000001 (Constraint Precision), scaling?: bool (default true: Use Automatic Scaling, GRG), ignoreIntegers?: bool (default false), integerOptimality?: percent (default 0), maxEvaluations?: 20000 (GridCraft counts evaluations, not time or iterations), maxSubproblems?, maxFeasibleSolutions? (branch and bound; default no limit), convergence?: 0.0001 (GRG and Evolutionary), derivatives?: forward|central (default central), multistart?: bool (default false: GRG from populationSize starting points, sharing maxEvaluations), keep?: bool (default true; false restores the original values), saveModel?: bool (default true: hidden solver_* names, options included), saved?: bool (solve the sheet's saved model; other params override), reports?: [answer|sensitivity|limits] (written on new sheets when a solution is found)} → {status: optimal|converged|cannotImprove|iterationLimit|infeasible|unbounded|notLinear, message, objective, values: {cell: value}, original: {cell: value}, model: the model as params, evaluations, changed, reports?: [sheet names]}. No `objective` → {model: the saved model} (the menu opens the dialog).",
            has_doc,
            run
        ),
        cmd!(
            "data.solverReport",
            "Solver Reports",
            [],
            None,
            "{reports: [answer|sensitivity|limits], values?: {cell: number} (the solution; default the variable cells' current values), original?: {cell: number} (the values before solving; default `values`), message?: text (the result line), model?: data.solver params (default the sheet's saved model, with any data.solver params given here over it)} → {sheets: [names]}. Writes Answer, Sensitivity and Limits reports on new sheets before the model's sheet; Sensitivity and Limits need Simplex LP or GRG Nonlinear and no integer constraints (or ignoreIntegers). Sensitivity: Simplex LP gives reduced costs, shadow prices and allowable increases and decreases (1E+30: no limit); GRG gives reduced gradients and Lagrange multipliers. data.solver's result can be passed as is.",
            has_doc,
            report
        ),
    ]
}

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, Debug, PartialEq)]
enum Rel {
    Le,
    Eq,
    Ge,
    Int,
    Bin,
}

impl Rel {
    fn parse(s: &str) -> Option<Rel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "<=" | "≤" | "=<" | "le" => Some(Rel::Le),
            ">=" | "≥" | "=>" | "ge" => Some(Rel::Ge),
            "=" | "==" | "eq" => Some(Rel::Eq),
            "int" | "integer" => Some(Rel::Int),
            "bin" | "binary" => Some(Rel::Bin),
            _ => None,
        }
    }
    fn text(self) -> &'static str {
        match self {
            Rel::Le => "<=",
            Rel::Eq => "=",
            Rel::Ge => ">=",
            Rel::Int => "int",
            Rel::Bin => "bin",
        }
    }
    /// The relation code of the saved model (`solver_relN`), as in Solver's VBA `SolverAdd`.
    fn code(self) -> u8 {
        match self {
            Rel::Le => 1,
            Rel::Eq => 2,
            Rel::Ge => 3,
            Rel::Int => 4,
            Rel::Bin => 5,
        }
    }
    fn from_code(c: &str) -> Option<Rel> {
        match c.trim() {
            "1" => Some(Rel::Le),
            "2" => Some(Rel::Eq),
            "3" => Some(Rel::Ge),
            "4" => Some(Rel::Int),
            "5" => Some(Rel::Bin),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum Rhs {
    None,
    Const(f64),
    Cells(Vec<CellRef>),
}

#[derive(Clone, Debug)]
struct Constraint {
    lhs: Vec<CellRef>,
    lhs_text: String,
    rel: Rel,
    rhs: Rhs,
    rhs_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Goal {
    Max,
    Min,
    Value(f64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Method {
    Simplex,
    Grg,
    Evolutionary,
}

/// Solver Options: All Methods, GRG Nonlinear and Evolutionary. The search keeps no clock
/// (`Instant` traps on wasm), so Max Time and Max Time without Improvement are counted in
/// evaluations and generations.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Options {
    /// Constraint Precision: how far a constraint may be off and still count as satisfied.
    precision: f64,
    /// Use Automatic Scaling: GRG Nonlinear steps each variable relative to its size.
    scaling: bool,
    /// Ignore Integer Constraints: integer and binary constraints are relaxed (binary keeps 0..1).
    ignore_integers: bool,
    /// Integer Optimality, as a fraction: branch and bound stops within this gap of the bound.
    gap: f64,
    max_evals: u64,
    /// Max Subproblems and Max Feasible Solutions of branch and bound (`None`: no limit).
    max_subproblems: Option<usize>,
    max_feasible: Option<usize>,
    /// Convergence (GRG Nonlinear and Evolutionary).
    convergence: f64,
    /// Derivatives: central differences (else forward).
    central: bool,
    /// GRG Nonlinear › Use Multistart: GRG from `population` starting points.
    multistart: bool,
    population: usize,
    mutation: f64,
    seed: u64,
    /// Evolutionary: generations without improvement before stopping.
    stall: u64,
    require_bounds: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            precision: PRECISION,
            scaling: true,
            ignore_integers: false,
            gap: 0.0,
            max_evals: DEFAULT_EVALS,
            max_subproblems: None,
            max_feasible: None,
            convergence: 1e-4,
            central: true,
            multistart: false,
            population: 100,
            mutation: 0.075,
            seed: 0,
            stall: 50,
            require_bounds: true,
        }
    }
}

/// The option params at their defaults (what Solver Options › Reset All shows).
pub fn option_defaults() -> Json {
    options_params(&Options::default())
}

fn options_params(o: &Options) -> Json {
    json!({
        "precision": o.precision,
        "scaling": o.scaling,
        "ignoreIntegers": o.ignore_integers,
        "integerOptimality": o.gap * 100.0,
        "maxEvaluations": o.max_evals,
        "maxSubproblems": o.max_subproblems,
        "maxFeasibleSolutions": o.max_feasible,
        "convergence": o.convergence,
        "derivatives": if o.central { "central" } else { "forward" },
        "multistart": o.multistart,
        "populationSize": o.population,
        "mutationRate": o.mutation,
        "randomSeed": o.seed,
        "generationsWithoutImprovement": o.stall,
        "requireBounds": o.require_bounds,
    })
}

fn options(p: &Json) -> Result<Options> {
    let d = Options::default();
    let num = |k: &str| p.get(k).and_then(Json::as_f64).or_else(|| str_param(p, k).and_then(num_text)).filter(|v| v.is_finite());
    // A blank or missing limit is no limit.
    let limit = |k: &str| num(k).filter(|v| *v >= 1.0 && *v < 1e9).map(|v| v as usize);
    let central = match str_param(p, "derivatives").map(|t| t.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => d.central,
        Some("central") => true,
        Some("forward") => false,
        Some(t) => return Err(bad(CMD, format!("unknown derivatives `{t}` (forward or central)"))),
    };
    Ok(Options {
        precision: num("precision").filter(|v| *v > 0.0).map_or(d.precision, |v| v.clamp(1e-15, 0.5)),
        scaling: bool_param(p, "scaling").unwrap_or(d.scaling),
        ignore_integers: bool_param(p, "ignoreIntegers").unwrap_or(d.ignore_integers),
        gap: num("integerOptimality").map_or(d.gap, |v| v.clamp(0.0, 100.0) / 100.0),
        max_evals: num("maxEvaluations").filter(|v| *v >= 1.0).map_or(d.max_evals, |v| (v as u64).min(MAX_EVALS)),
        max_subproblems: limit("maxSubproblems"),
        max_feasible: limit("maxFeasibleSolutions"),
        convergence: num("convergence").map_or(d.convergence, |v| v.clamp(0.0, 1.0)),
        central,
        multistart: bool_param(p, "multistart").unwrap_or(d.multistart),
        population: num("populationSize").map_or(d.population, |v| v.clamp(MIN_POPULATION as f64, MAX_POPULATION as f64) as usize),
        mutation: num("mutationRate").map_or(d.mutation, |v| v.clamp(0.0, 1.0)),
        seed: num("randomSeed").map_or(d.seed, |v| v.clamp(0.0, f64::from(u32::MAX)) as u64),
        stall: num("generationsWithoutImprovement").map_or(d.stall, |v| v.clamp(1.0, 100_000.0) as u64),
        require_bounds: bool_param(p, "requireBounds").unwrap_or(d.require_bounds),
    })
}

#[derive(Clone, Debug)]
struct Model {
    sheet: usize,
    objective: CellRef,
    goal: Goal,
    vars: Vec<CellRef>,
    vars_text: String,
    cons: Vec<Constraint>,
    non_negative: bool,
    method: Method,
    opt: Options,
}

/// Cells of `"B2:B4,D1"` on one sheet (row-major per area, in order), at most `cap`.
fn cells_of(wb: &Workbook, t: &str, default_sheet: usize, cap: usize) -> Result<(usize, Vec<CellRef>)> {
    let mut sheet = None;
    let mut out = Vec::new();
    for part in t.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        let (sh, body) = split_sheet(part);
        let si = match sh {
            Some(n) => wb.sheet_index(&n).ok_or_else(|| bad(CMD, format!("no sheet named `{n}`")))?,
            None => default_sheet,
        };
        if *sheet.get_or_insert(si) != si {
            return Err(bad(CMD, "all cells of a reference must be on one sheet"));
        }
        let r = RangeRef::parse(body.trim()).ok_or_else(|| bad(CMD, format!("not a reference: `{part}`")))?;
        if r.count() > cap as u64 || out.len() + r.count() as usize > cap {
            return Err(bad(CMD, format!("too many cells (at most {cap})")));
        }
        out.extend(r.iter());
    }
    if out.is_empty() {
        return Err(bad(CMD, format!("not a reference: `{t}`")));
    }
    Ok((sheet.unwrap_or(default_sheet), out))
}

fn num_text(t: &str) -> Option<f64> {
    gridcraft_core::parse::parse_number_text(t.trim()).filter(|v| v.is_finite())
}

/// Splits `"B5 <= 10"` / `"B2 int"` into (lhs, relation, rhs).
fn split_constraint(t: &str) -> Option<(String, Rel, String)> {
    for op in ["<=", ">=", "=<", "=>", "≤", "≥", "=="] {
        if let Some((a, b)) = t.split_once(op) {
            return Some((a.trim().into(), Rel::parse(op)?, b.trim().into()));
        }
    }
    if let Some((a, b)) = t.split_once('=') {
        return Some((a.trim().into(), Rel::Eq, b.trim().into()));
    }
    let lower = t.trim().to_ascii_lowercase();
    for (suffix, rel) in [("integer", Rel::Int), ("binary", Rel::Bin), ("int", Rel::Int), ("bin", Rel::Bin)] {
        if let Some(a) = lower.strip_suffix(suffix) {
            let a = a.trim();
            return Some((t.trim().get(..a.len())?.trim().to_string(), rel, String::new()));
        }
    }
    None
}

fn parse_constraint(wb: &Workbook, sheet: usize, c: &Json) -> Result<Constraint> {
    let (lhs_text, rel, rhs_json) = match c {
        Json::String(t) => {
            let (l, r, rhs) = split_constraint(t).ok_or_else(|| bad(CMD, format!("not a constraint: `{t}`")))?;
            (l, r, Json::String(rhs))
        }
        Json::Object(_) => {
            let lhs = str_param(c, "lhs").or_else(|| str_param(c, "cell")).or_else(|| str_param(c, "range")).unwrap_or("").to_string();
            let op = str_param(c, "op").or_else(|| str_param(c, "relation")).unwrap_or("");
            let rel = Rel::parse(op).ok_or_else(|| bad(CMD, format!("unknown constraint relation `{op}`")))?;
            (lhs, rel, c.get("rhs").or_else(|| c.get("value")).cloned().unwrap_or(Json::Null))
        }
        _ => return Err(bad(CMD, "a constraint is a string like \"B5 <= 10\" or {lhs, op, rhs}")),
    };
    let (lsi, lhs) = cells_of(wb, &lhs_text, sheet, MAX_CONSTRAINTS)?;
    if lsi != sheet {
        return Err(EngineError::Other("Constraint cells must be on the objective cell's sheet.".into()));
    }
    let (rhs, rhs_text) = match rel {
        Rel::Int | Rel::Bin => (Rhs::None, String::new()),
        _ => match &rhs_json {
            Json::Number(n) => {
                let v = n.as_f64().filter(|v| v.is_finite()).ok_or_else(|| bad(CMD, "the constraint's right side must be a number"))?;
                (Rhs::Const(v), v.to_string())
            }
            Json::String(t) => {
                let t = t.trim().trim_start_matches('=').trim();
                if let Some(v) = num_text(t) {
                    (Rhs::Const(v), t.to_string())
                } else {
                    let (rsi, cells) = cells_of(wb, t, sheet, MAX_CONSTRAINTS)?;
                    if rsi != sheet {
                        return Err(EngineError::Other("Constraint cells must be on the objective cell's sheet.".into()));
                    }
                    if cells.len() != 1 && cells.len() != lhs.len() {
                        return Err(EngineError::Other("The constraint's two sides must be the same size (or the right side one cell).".into()));
                    }
                    (Rhs::Cells(cells), t.to_string())
                }
            }
            _ => return Err(bad(CMD, "missing the constraint's right side")),
        },
    };
    Ok(Constraint { lhs, lhs_text, rel, rhs, rhs_text })
}

fn parse_model(s: &Session, p: &Json) -> Result<Model> {
    let d = s.doc()?;
    let wb = &d.wb;
    let obj_text = str_param(p, "objective").or_else(|| str_param(p, "setObjective")).or_else(|| str_param(p, "set")).unwrap_or("");
    let (sheet, obj) = cells_of(wb, obj_text, wb.active_sheet, 1).map_err(|_| EngineError::Other("The objective must be a single cell.".into()))?;
    let objective = *obj.first().ok_or_else(|| EngineError::Other("The objective must be a single cell.".into()))?;
    let sh = wb.sheet(sheet).ok_or(EngineError::NoDocument)?;
    if sh.cell(objective).is_none_or(|c| c.formula.is_none()) {
        return Err(EngineError::Other("The objective cell must contain a formula.".into()));
    }
    let goal = match str_param(p, "goal").or_else(|| str_param(p, "to")).unwrap_or("max").to_ascii_lowercase().as_str() {
        "max" | "maximize" | "maximum" => Goal::Max,
        "min" | "minimize" | "minimum" => Goal::Min,
        "value" | "valueof" | "value of" => {
            let v = f64_param(p, "value")
                .or_else(|| str_param(p, "value").and_then(num_text))
                .filter(|v| v.is_finite())
                .ok_or_else(|| bad(CMD, "goal `value` needs a number in `value`"))?;
            Goal::Value(v)
        }
        g => return Err(bad(CMD, format!("unknown goal `{g}` (max, min or value)"))),
    };
    let vars_text = str_param(p, "variables").or_else(|| str_param(p, "changing")).or_else(|| str_param(p, "byChanging")).unwrap_or("").to_string();
    let (vsi, mut vars) = cells_of(wb, &vars_text, sheet, MAX_VARS).map_err(|e| match e {
        EngineError::BadParams { msg, .. } if msg.starts_with("too many") => {
            EngineError::Other(format!("Too many variable cells (at most {MAX_VARS})."))
        }
        _ => EngineError::Other("The variable cells must be a reference like B2:B4.".into()),
    })?;
    if vsi != sheet {
        return Err(EngineError::Other("The variable cells must be on the objective cell's sheet.".into()));
    }
    let mut seen = std::collections::HashSet::new();
    vars.retain(|c| seen.insert(*c));
    if vars.iter().any(|c| sh.cell(*c).is_some_and(|c| c.formula.is_some())) {
        return Err(EngineError::Other("Variable cells must hold constants, not formulas.".into()));
    }
    let mut cons = Vec::new();
    let mut scalar = 0usize;
    if let Some(list) = p.get("constraints") {
        let list = list.as_array().ok_or_else(|| bad(CMD, "`constraints` must be an array"))?;
        if list.len() > MAX_CONSTRAINTS {
            return Err(EngineError::Other(format!("Too many constraints (at most {MAX_CONSTRAINTS}).")));
        }
        for c in list {
            let c = parse_constraint(wb, sheet, c)?;
            scalar = scalar.saturating_add(c.lhs.len());
            if scalar > MAX_CONSTRAINTS {
                return Err(EngineError::Other(format!("Too many constraints (at most {MAX_CONSTRAINTS} cells).")));
            }
            if matches!(c.rel, Rel::Int | Rel::Bin) && c.lhs.iter().any(|x| !vars.contains(x)) {
                return Err(EngineError::Other("Integer and binary constraints must be on variable cells.".into()));
            }
            cons.push(c);
        }
    }
    let non_negative = bool_param(p, "nonNegative").unwrap_or(true);
    let method = match str_param(p, "method").unwrap_or("grg").to_ascii_lowercase().replace([' ', '_', '-'], "").as_str() {
        "grg" | "grgnonlinear" | "nonlinear" => Method::Grg,
        "simplex" | "simplexlp" | "lp" | "linear" => Method::Simplex,
        "evolutionary" | "evo" | "genetic" => Method::Evolutionary,
        m => return Err(bad(CMD, format!("unknown method `{m}` (grg, simplex or evolutionary)"))),
    };
    Ok(Model { sheet, objective, goal, vars, vars_text, cons, non_negative, method, opt: options(p)? })
}

// ---------------------------------------------------------------- saved model

fn ref_text(sheet_name: &str, cells: &str) -> String {
    let q = crate::pivot::quote_sheet(sheet_name);
    cells
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .filter_map(|part| {
            let r = RangeRef::parse(split_sheet(part).1.trim())?;
            let abs = |c: CellRef| format!("${}${}", gridcraft_core::col_to_letters(c.col), c.row as u64 + 1);
            Some(if r.is_single() { format!("{q}!{}", abs(r.start)) } else { format!("{q}!{}:{}", abs(r.start), abs(r.end)) })
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The model as hidden names scoped to its sheet (replacing any saved before).
fn save_model(wb: &mut Workbook, m: &Model) {
    let Some(sheet_name) = wb.sheet(m.sheet).map(|s| s.name.clone()) else { return };
    wb.names.retain(|n| !(n.scope == Some(m.sheet) && n.name.to_ascii_lowercase().starts_with("solver_")));
    let mut put = |name: String, formula: String| {
        wb.names.push(DefinedName { name, scope: Some(m.sheet), formula, comment: String::new(), hidden: true });
    };
    put("solver_opt".into(), ref_text(&sheet_name, &m.objective.a1()));
    let (typ, val) = match m.goal {
        Goal::Max => (1, 0.0),
        Goal::Min => (2, 0.0),
        Goal::Value(v) => (3, v),
    };
    put("solver_typ".into(), typ.to_string());
    put("solver_val".into(), val.to_string());
    put("solver_adj".into(), ref_text(&sheet_name, &m.vars_text));
    for (i, c) in m.cons.iter().enumerate() {
        let k = i + 1;
        put(format!("solver_lhs{k}"), ref_text(&sheet_name, &c.lhs_text));
        put(format!("solver_rel{k}"), c.rel.code().to_string());
        let rhs = match &c.rhs {
            Rhs::None => {
                if c.rel == Rel::Bin {
                    "\"binary\"".to_string()
                } else {
                    "\"integer\"".to_string()
                }
            }
            Rhs::Const(v) => v.to_string(),
            Rhs::Cells(_) => ref_text(&sheet_name, &c.rhs_text),
        };
        put(format!("solver_rhs{k}"), rhs);
    }
    put("solver_num".into(), m.cons.len().to_string());
    put("solver_neg".into(), if m.non_negative { "1" } else { "2" }.into());
    let eng = match m.method {
        Method::Grg => "1",
        Method::Simplex => "2",
        Method::Evolutionary => "3",
    };
    put("solver_eng".into(), eng.into());
    // Solver Options, under the names Excel's Solver saves them with.
    let o = &m.opt;
    let flag = |on: bool| if on { "1" } else { "2" }.to_string();
    put("solver_pre".into(), o.precision.to_string());
    put("solver_scl".into(), flag(o.scaling));
    put("solver_rlx".into(), flag(o.ignore_integers));
    put("solver_tol".into(), o.gap.to_string());
    // Excel's solver_itr counts iterations, not evaluations: GridCraft keeps its own name.
    put("solver_gc_evals".into(), o.max_evals.to_string());
    if let Some(v) = o.max_subproblems {
        put("solver_nod".into(), v.to_string());
    }
    if let Some(v) = o.max_feasible {
        put("solver_mip".into(), v.to_string());
    }
    put("solver_cvg".into(), o.convergence.to_string());
    put("solver_drv".into(), if o.central { "2" } else { "1" }.into());
    put("solver_msl".into(), flag(o.multistart));
    put("solver_ssz".into(), o.population.to_string());
    put("solver_mrt".into(), o.mutation.to_string());
    put("solver_rsd".into(), o.seed.to_string());
    put("solver_rbv".into(), flag(o.require_bounds));
    put("solver_ver".into(), "3".into());
}

/// The saved model of a sheet as `data.solver` params, if any.
pub fn saved_model(wb: &Workbook, sheet: usize) -> Option<Json> {
    let get = |n: &str| {
        wb.names
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(n) && (d.scope == Some(sheet) || d.scope.is_none()))
            .map(|d| d.formula.trim().trim_start_matches('=').to_string())
    };
    let local = |t: String| -> String { t.split(',').map(|p| split_sheet(p.trim()).1.replace('$', "")).collect::<Vec<_>>().join(",") };
    let objective = local(get("solver_opt")?);
    if objective.is_empty() {
        return None;
    }
    let goal = match get("solver_typ").as_deref().map(str::trim) {
        Some("2") => "min",
        Some("3") => "value",
        _ => "max",
    };
    let value = get("solver_val").and_then(|v| num_text(&v)).unwrap_or(0.0);
    let count = get("solver_num").and_then(|v| num_text(&v)).unwrap_or(0.0).clamp(0.0, MAX_CONSTRAINTS as f64) as usize;
    let mut constraints = Vec::new();
    for k in 1..=count {
        let (Some(lhs), Some(rel)) = (get(&format!("solver_lhs{k}")), get(&format!("solver_rel{k}")).and_then(|r| Rel::from_code(&r))) else {
            continue;
        };
        let lhs = local(lhs);
        constraints.push(match rel {
            Rel::Int | Rel::Bin => json!(format!("{lhs} {}", rel.text())),
            _ => {
                let rhs = get(&format!("solver_rhs{k}")).unwrap_or_default();
                let rhs = if num_text(&rhs).is_some() { rhs } else { local(rhs) };
                json!(format!("{lhs} {} {rhs}", rel.text()))
            }
        });
    }
    let method = match get("solver_eng").as_deref().map(str::trim) {
        Some("2") => "simplex",
        Some("3") => "evolutionary",
        _ => "grg",
    };
    let mut model = json!({
        "objective": objective,
        "goal": goal,
        "value": value,
        "variables": get("solver_adj").map(local).unwrap_or_default(),
        "constraints": constraints,
        "nonNegative": get("solver_neg").as_deref().map(str::trim) != Some("2"),
        "method": method,
    });
    let number = |n: &str| get(n).and_then(|v| num_text(&v));
    for (name, key) in [
        ("solver_pre", "precision"),
        ("solver_cvg", "convergence"),
        ("solver_ssz", "populationSize"),
        ("solver_mrt", "mutationRate"),
        ("solver_rsd", "randomSeed"),
    ] {
        if let Some(v) = number(name) {
            model[key] = json!(v);
        }
    }
    // Integer Optimality is saved as a fraction and given as a percentage.
    if let Some(v) = number("solver_tol") {
        model["integerOptimality"] = json!(v * 100.0);
    }
    if let Some(v) = number("solver_gc_evals").filter(|v| *v >= 1.0 && *v <= MAX_EVALS as f64) {
        model["maxEvaluations"] = json!(v);
    }
    // The branch-and-bound limits: Excel saves "no limit" as a huge number.
    for (name, key) in [("solver_nod", "maxSubproblems"), ("solver_mip", "maxFeasibleSolutions")] {
        if let Some(v) = number(name).filter(|v| *v >= 1.0 && *v < 1e9) {
            model[key] = json!(v);
        }
    }
    for (name, key) in [("solver_scl", "scaling"), ("solver_rlx", "ignoreIntegers"), ("solver_msl", "multistart"), ("solver_rbv", "requireBounds")] {
        if let Some(v) = get(name) {
            model[key] = json!(v.trim() == "1");
        }
    }
    if let Some(v) = get("solver_drv") {
        model["derivatives"] = json!(if v.trim() == "1" { "forward" } else { "central" });
    }
    Some(model)
}

/// The model as `data.solver` params, in the form [`saved_model`] returns (the objective
/// qualified by its sheet).
fn model_params(wb: &Workbook, m: &Model) -> Json {
    let sheet = wb.sheet(m.sheet).map(|s| crate::pivot::quote_sheet(&s.name)).unwrap_or_default();
    let (goal, value) = match m.goal {
        Goal::Max => ("max", 0.0),
        Goal::Min => ("min", 0.0),
        Goal::Value(v) => ("value", v),
    };
    let constraints: Vec<Json> = m
        .cons
        .iter()
        .map(|c| match c.rel {
            Rel::Int | Rel::Bin => json!(format!("{} {}", c.lhs_text, c.rel.text())),
            _ => json!(format!("{} {} {}", c.lhs_text, c.rel.text(), c.rhs_text)),
        })
        .collect();
    let method = match m.method {
        Method::Simplex => "simplex",
        Method::Grg => "grg",
        Method::Evolutionary => "evolutionary",
    };
    let mut out = json!({
        "objective": format!("{sheet}!{}", m.objective.a1()),
        "goal": goal,
        "value": value,
        "variables": m.vars_text,
        "constraints": constraints,
        "nonNegative": m.non_negative,
        "method": method,
    });
    if let (Some(out), Json::Object(opts)) = (out.as_object_mut(), options_params(&m.opt)) {
        out.extend(opts);
    }
    out
}

// ---------------------------------------------------------------- evaluation

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sense {
    Le,
    Ge,
    Eq,
}

/// A scalar constraint `lhs (sense) rhs`.
#[derive(Clone, Debug)]
struct Row {
    lhs: CellRef,
    sense: Sense,
    rhs: Option<CellRef>,
    rhs_const: f64,
}

/// The values the solvers see at a point: the objective and `lhs − rhs` per row.
#[derive(Clone, Debug)]
struct Point {
    f: f64,
    g: Vec<f64>,
}

struct Evaluator {
    trial: Trial,
    sheet: usize,
    vars: Vec<CellRef>,
    objective: CellRef,
    rows: Vec<Row>,
    evals: u64,
    budget: u64,
    /// The Limits report: the objective is this variable itself.
    target: Option<usize>,
}

fn as_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => *n,
        Value::Empty => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => f64::NAN,
    }
}

impl Evaluator {
    fn exhausted(&self) -> bool {
        self.evals >= self.budget
    }

    /// Evaluates the model at `x`; `None` once the budget is spent.
    fn eval(&mut self, x: &[f64]) -> Option<Point> {
        if self.exhausted() {
            return None;
        }
        self.evals += 1;
        let sets: Vec<(usize, CellRef, Value)> = self.vars.iter().zip(x).map(|(c, v)| (self.sheet, *c, Value::number(*v))).collect();
        let mut reads = Vec::with_capacity(1 + 2 * self.rows.len());
        reads.push((self.sheet, self.objective));
        for r in &self.rows {
            reads.push((self.sheet, r.lhs));
            if let Some(c) = r.rhs {
                reads.push((self.sheet, c));
            }
        }
        let vals = self.trial.run(&sets, &reads);
        let mut it = vals.iter().map(as_num);
        let f = it.next().unwrap_or(f64::NAN);
        let f = match self.target {
            Some(j) => x.get(j).copied().unwrap_or(f64::NAN),
            None => f,
        };
        let mut g = Vec::with_capacity(self.rows.len());
        for r in &self.rows {
            let l = it.next().unwrap_or(f64::NAN);
            let rv = if r.rhs.is_some() { it.next().unwrap_or(f64::NAN) } else { r.rhs_const };
            g.push(l - rv);
        }
        Some(Point { f, g })
    }
}

// ---------------------------------------------------------------- problem

struct Problem {
    lo: Vec<f64>,
    hi: Vec<f64>,
    integer: Vec<bool>,
    rows: Vec<Row>,
}

/// Splits the constraints into variable bounds and general rows.
fn build_problem(m: &Model) -> Problem {
    let n = m.vars.len();
    let mut lo = vec![f64::NEG_INFINITY; n];
    let mut hi = vec![f64::INFINITY; n];
    let mut has_lower = vec![false; n];
    let mut integer = vec![false; n];
    let mut rows = Vec::new();
    let var_index = |c: &CellRef| m.vars.iter().position(|v| v == c);
    for c in &m.cons {
        for (k, cell) in c.lhs.iter().enumerate() {
            let j = var_index(cell);
            match c.rel {
                Rel::Int | Rel::Bin => {
                    if let Some(j) = j {
                        // Ignore Integer Constraints relaxes them; a binary variable keeps 0..1.
                        if let Some(b) = integer.get_mut(j) {
                            *b = !m.opt.ignore_integers;
                        }
                        if c.rel == Rel::Bin
                            && let (Some(l), Some(h), Some(hl)) = (lo.get_mut(j), hi.get_mut(j), has_lower.get_mut(j))
                        {
                            *l = l.max(0.0);
                            *h = h.min(1.0);
                            *hl = true;
                        }
                    }
                }
                rel => {
                    let sense = match rel {
                        Rel::Le => Sense::Le,
                        Rel::Ge => Sense::Ge,
                        _ => Sense::Eq,
                    };
                    let (rhs, rhs_const) = match &c.rhs {
                        Rhs::Const(v) => (None, *v),
                        Rhs::Cells(cells) => (cells.get(k).or_else(|| cells.first()).copied(), 0.0),
                        Rhs::None => (None, 0.0),
                    };
                    match (j, rhs) {
                        // A constant bound on a variable cell is a box bound.
                        (Some(j), None) => {
                            if let (Some(l), Some(h), Some(hl)) = (lo.get_mut(j), hi.get_mut(j), has_lower.get_mut(j)) {
                                if sense != Sense::Ge {
                                    *h = h.min(rhs_const);
                                }
                                if sense != Sense::Le {
                                    *l = l.max(rhs_const);
                                    *hl = true;
                                }
                            }
                        }
                        _ => rows.push(Row { lhs: *cell, sense, rhs, rhs_const }),
                    }
                }
            }
        }
    }
    if m.non_negative {
        for (l, hl) in lo.iter_mut().zip(&has_lower) {
            if !hl {
                *l = l.max(0.0);
            }
        }
    }
    Problem { lo, hi, integer, rows }
}

/// What a relaxation (or the whole solve) ended with.
#[derive(Clone, Debug)]
enum Outcome {
    /// x, objective to minimise, and whether the solver converged (vs. ran out of budget).
    Solved(Vec<f64>, f64, bool),
    Infeasible,
    Unbounded,
    Limit,
}

/// Which limit stopped a solve before it finished.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stop {
    Evaluations,
    Subproblems,
    FeasibleSolutions,
}

/// Branch and bound's limits: subproblems (relaxations solved) and integer solutions found.
#[derive(Clone, Copy, Debug)]
struct BnbLimits {
    nodes: usize,
    /// Max Subproblems was set (else `nodes` is GridCraft's own cap).
    user_nodes: bool,
    feasible: Option<usize>,
    gap: f64,
}

impl BnbLimits {
    fn new(cap: usize, o: &Options) -> BnbLimits {
        BnbLimits {
            nodes: o.max_subproblems.map_or(cap, |v| v.min(cap)),
            user_nodes: o.max_subproblems.is_some(),
            feasible: o.max_feasible,
            gap: o.gap,
        }
    }
}

/// Depth-first branch and bound over a relaxation solver; returns the best integer point and,
/// when a Solver Options limit stopped it, which.
fn branch_and_bound(
    lo: &[f64],
    hi: &[f64],
    integer: &[bool],
    limits: BnbLimits,
    relax: &mut dyn FnMut(&[f64], &[f64]) -> Outcome,
) -> (Outcome, Option<Stop>) {
    let gap = limits.gap;
    let mut stack: Vec<(Vec<f64>, Vec<f64>)> = vec![(lo.to_vec(), hi.to_vec())];
    let mut best: Option<(Vec<f64>, f64)> = None;
    let mut nodes = 0usize;
    let mut found = 0usize;
    let mut limited = false;
    let mut stop = None;
    let mut root = true;
    while let Some((l, h)) = stack.pop() {
        if nodes >= limits.nodes {
            limited = true;
            if limits.user_nodes {
                stop = Some(Stop::Subproblems);
            }
            break;
        }
        if limits.feasible.is_some_and(|k| found >= k) {
            limited = true;
            stop = Some(Stop::FeasibleSolutions);
            break;
        }
        nodes += 1;
        let out = relax(&l, &h);
        let was_root = std::mem::replace(&mut root, false);
        let (x, obj, converged) = match out {
            Outcome::Solved(x, obj, c) => (x, obj, c),
            Outcome::Unbounded if was_root => return (Outcome::Unbounded, None),
            Outcome::Limit => {
                limited = true;
                break;
            }
            _ => continue,
        };
        if !converged {
            limited = true;
        }
        if let Some((_, b)) = &best
            && obj >= b - gap * b.abs().max(1e-12) - 1e-9 * (1.0 + b.abs())
        {
            continue;
        }
        // The most fractional integer variable.
        let frac = x
            .iter()
            .zip(integer)
            .enumerate()
            .filter(|(_, (_, int))| **int)
            .map(|(j, (v, _))| (j, (v - v.round()).abs()))
            .filter(|(_, f)| *f > 1e-6)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match frac {
            None => {
                let snapped: Vec<f64> = x.iter().zip(integer).map(|(v, int)| if *int { v.round() } else { *v }).collect();
                best = Some((snapped, obj));
                found += 1;
            }
            Some((j, _)) => {
                let v = x.get(j).copied().unwrap_or(0.0);
                let (mut down_h, mut up_l) = (h.clone(), l.clone());
                if let Some(e) = down_h.get_mut(j) {
                    *e = e.min(v.floor());
                }
                if let Some(e) = up_l.get_mut(j) {
                    *e = e.max(v.ceil());
                }
                // Explore the nearer side first (pushed last).
                if v - v.floor() < 0.5 {
                    stack.push((up_l, h));
                    stack.push((l, down_h));
                } else {
                    stack.push((l, down_h));
                    stack.push((up_l, h));
                }
            }
        }
    }
    match best {
        Some((x, obj)) => (Outcome::Solved(x, obj, !limited), stop),
        None if limited => (Outcome::Limit, stop),
        None => (Outcome::Infeasible, None),
    }
}

// ---------------------------------------------------------------- simplex

/// Pivots the tableau on (r, j), updating the objective row.
fn pivot(t: &mut [Vec<f64>], obj: &mut [f64], r: usize, j: usize) {
    let Some(prow) = t.get_mut(r) else { return };
    let pv = prow.get(j).copied().unwrap_or(0.0);
    if pv == 0.0 {
        return;
    }
    for v in prow.iter_mut() {
        *v /= pv;
    }
    let prow = prow.clone();
    for (i, row) in t.iter_mut().enumerate() {
        if i == r {
            continue;
        }
        let f = row.get(j).copied().unwrap_or(0.0);
        if f != 0.0 {
            for (v, p) in row.iter_mut().zip(&prow) {
                *v -= f * p;
            }
        }
    }
    let f = obj.get(j).copied().unwrap_or(0.0);
    if f != 0.0 {
        for (v, p) in obj.iter_mut().zip(&prow) {
            *v -= f * p;
        }
    }
}

enum Step {
    Optimal,
    Unbounded,
    Limit,
}

/// Minimises with the reduced costs in `obj` (last entry: −objective). Columns not `allowed`
/// never enter.
fn simplex(t: &mut [Vec<f64>], basis: &mut [usize], obj: &mut [f64], allowed: &[bool], budget: &mut u64) -> Step {
    let n = obj.len().saturating_sub(1);
    let cost = (t.len() as u64 + 1).saturating_mul(n as u64 + 1).max(1);
    let mut stalled = 0u32;
    loop {
        if *budget < cost {
            return Step::Limit;
        }
        *budget -= cost;
        let bland = stalled > 50;
        let mut enter = None;
        let mut best = -EPS;
        for (j, d) in obj.iter().take(n).enumerate() {
            if !allowed.get(j).copied().unwrap_or(false) || *d >= -EPS {
                continue;
            }
            if bland {
                enter = Some(j);
                break;
            }
            if *d < best {
                best = *d;
                enter = Some(j);
            }
        }
        let Some(j) = enter else { return Step::Optimal };
        let mut leave: Option<(usize, f64)> = None;
        for (i, row) in t.iter().enumerate() {
            let a = row.get(j).copied().unwrap_or(0.0);
            if a <= EPS {
                continue;
            }
            let ratio = row.get(n).copied().unwrap_or(0.0) / a;
            let better = match leave {
                None => true,
                Some((li, lr)) => ratio < lr - 1e-12 || (ratio <= lr + 1e-12 && basis.get(i) < basis.get(li)),
            };
            if better {
                leave = Some((i, ratio));
            }
        }
        let Some((r, ratio)) = leave else { return Step::Unbounded };
        stalled = if ratio <= EPS { stalled + 1 } else { 0 };
        pivot(t, obj, r, j);
        if let Some(b) = basis.get_mut(r) {
            *b = j;
        }
    }
}

/// The final simplex tableau of a solved LP, for the Sensitivity report. The tableau works on
/// `y ≥ 0` with `x_var = offset + sign·y_k` per column `k`; its rows are the given rows (in
/// order), then the variables' upper bounds, each multiplied by `row_sign` so its right side is
/// non-negative.
struct LpFinal {
    t: Vec<Vec<f64>>,
    basis: Vec<usize>,
    /// Reduced costs (`width` columns, then −objective).
    obj: Vec<f64>,
    /// Per `y` column: (variable, sign).
    cols: Vec<(usize, f64)>,
    row_sign: Vec<f64>,
    /// Per row: a column that is a unit column of that row in the original tableau (its slack,
    /// or its artificial) and its coefficient there.
    unit: Vec<(usize, f64)>,
    width: usize,
    /// Columns that may not enter (artificials).
    art_from: usize,
}

/// Minimises `c·x` subject to `rows` (coefficients, sense, rhs) and `lo ≤ x ≤ hi`; `keep` receives
/// the final tableau.
fn solve_lp(c: &[f64], rows: &[(Vec<f64>, Sense, f64)], lo: &[f64], hi: &[f64], budget: &mut u64, keep: Option<&mut Option<LpFinal>>) -> Outcome {
    let n = c.len();
    if lo.iter().zip(hi).any(|(l, h)| l > h) {
        return Outcome::Infeasible;
    }
    // x_j = offset + Σ sign·y_k with y ≥ 0.
    struct Map {
        var: usize,
        sign: f64,
    }
    let mut cols: Vec<Map> = Vec::new();
    let mut offset = vec![0.0; n];
    let mut extra: Vec<(Vec<(usize, f64)>, Sense, f64)> = Vec::new();
    for j in 0..n {
        let (l, h) = (lo.get(j).copied().unwrap_or(f64::NEG_INFINITY), hi.get(j).copied().unwrap_or(f64::INFINITY));
        if l.is_finite() {
            if let Some(o) = offset.get_mut(j) {
                *o = l;
            }
            cols.push(Map { var: j, sign: 1.0 });
            if h.is_finite() {
                extra.push((vec![(cols.len() - 1, 1.0)], Sense::Le, h - l));
            }
        } else if h.is_finite() {
            if let Some(o) = offset.get_mut(j) {
                *o = h;
            }
            cols.push(Map { var: j, sign: -1.0 });
        } else {
            cols.push(Map { var: j, sign: 1.0 });
            cols.push(Map { var: j, sign: -1.0 });
        }
    }
    let ny = cols.len();
    // Rows over y: (coefficients, sense, rhs).
    let mut yrows: Vec<(Vec<f64>, Sense, f64)> = Vec::with_capacity(rows.len() + extra.len());
    for (a, sense, b) in rows {
        let mut coef = vec![0.0; ny];
        for (k, m) in cols.iter().enumerate() {
            if let Some(v) = coef.get_mut(k) {
                *v = a.get(m.var).copied().unwrap_or(0.0) * m.sign;
            }
        }
        let shift: f64 = a.iter().zip(&offset).map(|(x, o)| x * o).sum();
        yrows.push((coef, *sense, b - shift));
    }
    for (entries, sense, b) in extra {
        let mut coef = vec![0.0; ny];
        for (k, v) in entries {
            if let Some(e) = coef.get_mut(k) {
                *e = v;
            }
        }
        yrows.push((coef, sense, b));
    }
    let m = yrows.len();
    let n_slack = yrows.iter().filter(|r| r.1 != Sense::Eq).count();
    // Rows with b < 0 are negated so every right side is ≥ 0.
    let row_sign: Vec<f64> = yrows.iter().map(|r| if r.2 < 0.0 { -1.0 } else { 1.0 }).collect();
    let normalized: Vec<(Vec<f64>, Sense, f64)> = yrows
        .into_iter()
        .map(|(a, s, b)| {
            if b < 0.0 {
                let s = match s {
                    Sense::Le => Sense::Ge,
                    Sense::Ge => Sense::Le,
                    Sense::Eq => Sense::Eq,
                };
                (a.iter().map(|v| -v).collect(), s, -b)
            } else {
                (a, s, b)
            }
        })
        .collect();
    let n_art = normalized.iter().filter(|r| r.1 != Sense::Le).count();
    let width = ny + n_slack + n_art;
    let mut t: Vec<Vec<f64>> = Vec::with_capacity(m);
    let mut basis = vec![0usize; m];
    let mut unit = vec![(0usize, 1.0); m];
    let (mut si, mut ai) = (ny, ny + n_slack);
    for (i, (a, s, b)) in normalized.iter().enumerate() {
        let mut row = vec![0.0; width + 1];
        for (v, x) in row.iter_mut().zip(a) {
            *v = *x;
        }
        if let Some(u) = unit.get_mut(i) {
            *u = match s {
                Sense::Le => (si, 1.0),
                Sense::Ge => (si, -1.0),
                Sense::Eq => (ai, 1.0),
            };
        }
        match s {
            Sense::Le => {
                if let Some(v) = row.get_mut(si) {
                    *v = 1.0;
                }
                if let Some(bi) = basis.get_mut(i) {
                    *bi = si;
                }
                si += 1;
            }
            Sense::Ge => {
                if let Some(v) = row.get_mut(si) {
                    *v = -1.0;
                }
                si += 1;
                if let Some(v) = row.get_mut(ai) {
                    *v = 1.0;
                }
                if let Some(bi) = basis.get_mut(i) {
                    *bi = ai;
                }
                ai += 1;
            }
            Sense::Eq => {
                if let Some(v) = row.get_mut(ai) {
                    *v = 1.0;
                }
                if let Some(bi) = basis.get_mut(i) {
                    *bi = ai;
                }
                ai += 1;
            }
        }
        if let Some(v) = row.get_mut(width) {
            *v = *b;
        }
        t.push(row);
    }
    let is_art = |j: usize| j >= ny + n_slack && j < width;
    // Phase 1: minimise the sum of the artificials.
    let mut obj = vec![0.0; width + 1];
    for j in (ny + n_slack)..width {
        if let Some(v) = obj.get_mut(j) {
            *v = 1.0;
        }
    }
    for (i, row) in t.iter().enumerate() {
        if basis.get(i).is_some_and(|b| is_art(*b)) {
            for (v, x) in obj.iter_mut().zip(row) {
                *v -= x;
            }
        }
    }
    let all: Vec<bool> = vec![true; width];
    if n_art > 0 {
        match simplex(&mut t, &mut basis, &mut obj, &all, budget) {
            Step::Limit => return Outcome::Limit,
            Step::Unbounded | Step::Optimal => {}
        }
        let scale = normalized.iter().map(|r| r.2.abs()).fold(1.0f64, f64::max);
        if -obj.get(width).copied().unwrap_or(0.0) > 1e-7 * scale {
            return Outcome::Infeasible;
        }
        // Drive zero-valued artificials out of the basis where possible.
        for i in 0..m {
            if !basis.get(i).is_some_and(|b| is_art(*b)) {
                continue;
            }
            let j = t.get(i).and_then(|row| (0..ny + n_slack).find(|&j| row.get(j).is_some_and(|v| v.abs() > EPS)));
            if let Some(j) = j {
                pivot(&mut t, &mut obj, i, j);
                if let Some(b) = basis.get_mut(i) {
                    *b = j;
                }
            }
        }
    }
    // Phase 2: the real costs; artificials may not re-enter.
    let mut cost = vec![0.0; width + 1];
    for (k, mp) in cols.iter().enumerate() {
        if let Some(v) = cost.get_mut(k) {
            *v = c.get(mp.var).copied().unwrap_or(0.0) * mp.sign;
        }
    }
    let mut obj = cost.clone();
    for (i, row) in t.iter().enumerate() {
        let cb = basis.get(i).and_then(|b| cost.get(*b)).copied().unwrap_or(0.0);
        if cb != 0.0 {
            for (v, x) in obj.iter_mut().zip(row) {
                *v -= cb * x;
            }
        }
    }
    let allowed: Vec<bool> = (0..width).map(|j| !is_art(j)).collect();
    match simplex(&mut t, &mut basis, &mut obj, &allowed, budget) {
        Step::Limit => return Outcome::Limit,
        Step::Unbounded => return Outcome::Unbounded,
        Step::Optimal => {}
    }
    let mut y = vec![0.0; width];
    for (i, row) in t.iter().enumerate() {
        if let (Some(b), Some(v)) = (basis.get(i), row.get(width))
            && let Some(e) = y.get_mut(*b)
        {
            *e = *v;
        }
    }
    let mut x = offset;
    for (k, mp) in cols.iter().enumerate() {
        if let Some(e) = x.get_mut(mp.var) {
            *e += mp.sign * y.get(k).copied().unwrap_or(0.0);
        }
    }
    let val: f64 = c.iter().zip(&x).map(|(a, b)| a * b).sum();
    if let Some(keep) = keep {
        let cols = cols.iter().map(|mp| (mp.var, mp.sign)).collect();
        *keep = Some(LpFinal { t, basis, obj, cols, row_sign, unit, width, art_from: ny + n_slack });
    }
    Outcome::Solved(x, val, true)
}

/// Linearises the model at `x0` (n + 1 evaluations) and checks it at one more point. Returns the
/// objective gradient and constant, and per row the gradient and constant of `lhs − rhs`.
#[allow(clippy::type_complexity)]
fn linearize(ev: &mut Evaluator, x0: &[f64]) -> std::result::Result<((Vec<f64>, f64), Vec<(Vec<f64>, f64)>), &'static str> {
    let n = x0.len();
    let p0 = ev.eval(x0).ok_or("budget")?;
    if !p0.f.is_finite() || p0.g.iter().any(|v| !v.is_finite()) {
        return Err("error");
    }
    let mut cf = vec![0.0; n];
    let mut ga = vec![vec![0.0; n]; p0.g.len()];
    for j in 0..n {
        let mut x = x0.to_vec();
        if let Some(v) = x.get_mut(j) {
            *v += 1.0;
        }
        let p = ev.eval(&x).ok_or("budget")?;
        if let Some(v) = cf.get_mut(j) {
            *v = p.f - p0.f;
        }
        for (row, (gi, g0)) in ga.iter_mut().zip(p.g.iter().zip(&p0.g)) {
            if let Some(v) = row.get_mut(j) {
                *v = gi - g0;
            }
        }
    }
    let dot = |a: &[f64], x: &[f64]| a.iter().zip(x).map(|(p, q)| p * q).sum::<f64>();
    let f_const = p0.f - dot(&cf, x0);
    let rows: Vec<(Vec<f64>, f64)> = ga.into_iter().zip(&p0.g).map(|(a, g0)| (a.clone(), g0 - dot(&a, x0))).collect();
    // Linearity check at a spread-out point.
    let xt: Vec<f64> =
        x0.iter().enumerate().map(|(j, v)| v + if j % 2 == 0 { 1.7 + 0.3 * (j % 5) as f64 } else { -0.6 - 0.2 * (j % 3) as f64 }).collect();
    let pt = ev.eval(&xt).ok_or("budget")?;
    let close = |actual: f64, pred: f64, terms: f64| actual.is_finite() && (actual - pred).abs() <= 1e-6 * (1.0 + actual.abs() + terms);
    let terms = |a: &[f64]| a.iter().zip(&xt).map(|(p, q)| (p * q).abs()).sum::<f64>();
    if !close(pt.f, f_const + dot(&cf, &xt), terms(&cf)) {
        return Err("nonlinear");
    }
    for ((a, k), g) in rows.iter().zip(&pt.g) {
        if !close(*g, k + dot(a, &xt), terms(a)) {
            return Err("nonlinear");
        }
    }
    Ok(((cf, f_const), rows))
}

// ---------------------------------------------------------------- nonlinear

/// Augmented-Lagrangian value of a point: objective `phi` plus the constraint terms.
fn aug(phi: f64, cons: &[f64], senses: &[Sense], lam: &[f64], mu: f64) -> f64 {
    let mut v = phi;
    for ((c, s), l) in cons.iter().zip(senses).zip(lam) {
        v += match s {
            Sense::Eq => l * c + 0.5 * mu * c * c,
            _ => {
                let t = (l + mu * c).max(0.0);
                (t * t - l * l) / (2.0 * mu)
            }
        };
    }
    v
}

/// Constraint values in the `≤ 0` / `= 0` form, and the objective to minimise.
fn shape(p: &Point, rows: &[Row], goal: Goal) -> (f64, Vec<f64>) {
    let phi = match goal {
        Goal::Max => -p.f,
        Goal::Min => p.f,
        Goal::Value(v) => (p.f - v) * (p.f - v),
    };
    let cons = p.g.iter().zip(rows).map(|(g, r)| if r.sense == Sense::Ge { -g } else { *g }).collect();
    (phi, cons)
}

fn violation(cons: &[f64], senses: &[Sense]) -> f64 {
    cons.iter().zip(senses).map(|(c, s)| if *s == Sense::Eq { c.abs() } else { c.max(0.0) }).fold(0.0, f64::max)
}

fn project(x: &mut [f64], lo: &[f64], hi: &[f64]) {
    for ((v, l), h) in x.iter_mut().zip(lo).zip(hi) {
        if *v < *l {
            *v = *l;
        }
        if *v > *h {
            *v = *h;
        }
    }
}

/// Local minimisation of the model within bounds `lo..hi` from `x0`.
fn nlp(ev: &mut Evaluator, x0: &[f64], lo: &[f64], hi: &[f64], goal: Goal, o: &Options) -> Outcome {
    let n = x0.len();
    let precision = o.precision;
    // Automatic scaling: steps in units of each variable's size (a diagonal preconditioner), so
    // variables of very different magnitudes move at comparable rates.
    let size: Vec<f64> = x0.iter().map(|v| if o.scaling && v.is_finite() { v.abs().max(1.0) } else { 1.0 }).collect();
    let weight: Vec<f64> = size.iter().map(|s| s * s).collect();
    let rows = ev.rows.clone();
    let senses: Vec<Sense> = rows.iter().map(|r| r.sense).collect();
    let mut x = x0.to_vec();
    project(&mut x, lo, hi);
    let mut lam = vec![0.0; rows.len()];
    let mut mu = 10.0;
    let Some(p) = ev.eval(&x) else { return Outcome::Limit };
    let (mut phi, mut cons) = shape(&p, &rows, goal);
    if !phi.is_finite() {
        return Outcome::Infeasible;
    }
    let phi0 = phi;
    let mut prev_viol = violation(&cons, &senses);
    let mut converged = false;
    let mut out_of_budget = false;
    let mut carried: Option<f64> = None;
    for _outer in 0..40 {
        // Inner: projected gradient on the augmented Lagrangian.
        let lval = |phi: f64, cons: &[f64], lam: &[f64], mu: f64| aug(phi, cons, &senses, lam, mu);
        // Central differences, or forward differences from the value `l0` at `x` (one evaluation
        // per variable instead of two).
        let grad = |ev: &mut Evaluator, x: &[f64], l0: f64, lam: &[f64], mu: f64| -> Option<Vec<f64>> {
            let mut gr = vec![0.0; n];
            for j in 0..n {
                let xj = x.get(j).copied().unwrap_or(0.0);
                let h = 1e-6 * xj.abs().max(1.0);
                let (l, u) = (lo.get(j).copied().unwrap_or(f64::NEG_INFINITY), hi.get(j).copied().unwrap_or(f64::INFINITY));
                let (a, b) = if o.central {
                    (if xj - h >= l { xj - h } else { xj }, if xj + h <= u { xj + h } else { xj })
                } else if xj + h <= u {
                    (xj, xj + h)
                } else if xj - h >= l {
                    (xj - h, xj)
                } else {
                    (xj, xj)
                };
                if b <= a {
                    continue;
                }
                let mut at = |t: f64| -> Option<f64> {
                    if t == xj {
                        return Some(l0);
                    }
                    let mut xt = x.to_vec();
                    if let Some(v) = xt.get_mut(j) {
                        *v = t;
                    }
                    let (p, c) = shape(&ev.eval(&xt)?, &rows, goal);
                    Some(lval(p, &c, lam, mu))
                };
                let (la, lb) = (at(a)?, at(b)?);
                let d = (lb - la) / (b - a);
                if let Some(v) = gr.get_mut(j) {
                    *v = if d.is_finite() { d } else { 0.0 };
                }
            }
            Some(gr)
        };
        let mut l_cur = lval(phi, &cons, &lam, mu);
        let Some(mut g) = grad(ev, &x, l_cur, &lam, mu) else {
            out_of_budget = true;
            break;
        };
        // The step length carries over between outer iterations once known.
        let gnorm = g.iter().zip(&weight).map(|(v, w)| (v * w).abs()).fold(0.0, f64::max);
        let mut alpha = carried.unwrap_or(if gnorm > 0.0 { 1.0 / gnorm } else { 1.0 });
        let mut inner_done = false;
        let mut quiet = 0;
        // Iterations in a row whose relative change in the objective is within Convergence.
        let mut slow = 0;
        for _inner in 0..500 {
            let mut accepted = None;
            let mut a = alpha;
            for _ in 0..40 {
                let mut xn: Vec<f64> = x.iter().zip(&g).zip(&weight).map(|((v, d), w)| v - a * w * d).collect();
                project(&mut xn, lo, hi);
                let Some(pn) = ev.eval(&xn) else { break };
                let (phn, cn) = shape(&pn, &rows, goal);
                let ln = lval(phn, &cn, &lam, mu);
                let decrease: f64 = g.iter().zip(x.iter().zip(&xn)).map(|(d, (u, v))| d * (u - v)).sum();
                if ln.is_finite() && ln <= l_cur - 1e-4 * decrease {
                    accepted = Some((xn, phn, cn, ln));
                    break;
                }
                a *= 0.25;
            }
            if ev.exhausted() {
                out_of_budget = true;
            }
            let Some((xn, phn, cn, ln)) = accepted else {
                inner_done = true;
                break;
            };
            // Variables or objective running off towards infinity: unbounded.
            if phn.abs() > 1e15 * (1.0 + phi0.abs()) || xn.iter().zip(x0).any(|(v, s)| v.abs() > 1e15 * (1.0 + s.abs())) {
                return Outcome::Unbounded;
            }
            let s: Vec<f64> = xn.iter().zip(&x).map(|(a, b)| a - b).collect();
            let step = s.iter().map(|v| v.abs()).fold(0.0, f64::max);
            let xmax = xn.iter().map(|v| v.abs()).fold(1.0, f64::max);
            let small = step <= 1e-10 * xmax || (l_cur - ln).abs() <= 1e-13 * (1.0 + l_cur.abs());
            quiet = if small { quiet + 1 } else { 0 };
            slow = if (l_cur - ln).abs() <= o.convergence * l_cur.abs().max(1.0) { slow + 1 } else { 0 };
            x = xn;
            phi = phn;
            cons = cn;
            l_cur = ln;
            if quiet >= 3 || slow >= 5 {
                inner_done = true;
                break;
            }
            let Some(gn) = grad(ev, &x, l_cur, &lam, mu) else {
                out_of_budget = true;
                break;
            };
            // Barzilai–Borwein step (in the scaled variables).
            let yv: Vec<f64> = gn.iter().zip(&g).map(|(a, b)| a - b).collect();
            let sy: f64 = s.iter().zip(&yv).map(|(a, b)| a * b).sum();
            let ss: f64 = s.iter().zip(&weight).map(|(v, w)| v * v / w).sum();
            alpha = if sy > 0.0 { (ss / sy).clamp(1e-12, 1e12) } else { (a * 4.0).min(1e12) };
            g = gn;
            if g.iter().all(|v| *v == 0.0) {
                inner_done = true;
                break;
            }
        }
        carried = Some(alpha);
        if out_of_budget {
            break;
        }
        let viol = violation(&cons, &senses);
        let scale = rows.iter().map(|r| r.rhs_const.abs()).fold(1.0, f64::max);
        if viol <= precision * scale && inner_done {
            converged = true;
            if rows.is_empty() {
                break;
            }
        }
        // Multiplier and penalty updates.
        for ((l, c), s) in lam.iter_mut().zip(&cons).zip(&senses) {
            *l = match s {
                Sense::Eq => *l + mu * c,
                _ => (*l + mu * c).max(0.0),
            };
        }
        if viol > 0.25 * prev_viol {
            mu = (mu * 10.0).min(1e10);
        }
        if converged && viol <= precision * scale * 1e-2 {
            break;
        }
        prev_viol = viol;
        converged = false;
    }
    let viol = violation(&cons, &senses);
    let scale = rows.iter().map(|r| r.rhs_const.abs()).fold(1.0, f64::max);
    if viol > precision * scale * 10.0 {
        return if out_of_budget { Outcome::Limit } else { Outcome::Infeasible };
    }
    Outcome::Solved(x, phi, (converged || viol <= precision * scale) && !out_of_budget)
}

// ---------------------------------------------------------------- evolutionary

/// A small deterministic generator (SplitMix64), so a seed reproduces a run on every platform.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x2545_F491_4F6C_DD1D)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn between(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.uniform()
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { usize::try_from(self.next() % n as u64).unwrap_or(0) }
    }
    /// Standard normal (Box–Muller).
    fn normal(&mut self) -> f64 {
        let u = self.uniform().max(f64::MIN_POSITIVE);
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * self.uniform()).cos()
    }
}

/// A candidate: its point, the objective to minimise and its constraint violation.
#[derive(Clone, Debug)]
struct Member {
    x: Vec<f64>,
    phi: f64,
    viol: f64,
}

impl Member {
    /// Feasible beats infeasible; then the lower objective, or the smaller violation.
    fn better(&self, o: &Member, tol: f64) -> bool {
        match (self.viol <= tol, o.viol <= tol) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => self.phi < o.phi,
            (false, false) => self.viol < o.viol,
        }
    }
}

/// Why the Evolutionary search stopped.
#[derive(Clone, Copy, Debug, PartialEq)]
enum EvoEnd {
    /// Almost all of the population is within Convergence of the best member.
    Converged,
    /// The best member didn't improve for the set number of generations.
    Stalled,
    /// The evaluation budget ran out.
    Budget,
}

/// The search box, integer flags and scoring shared by the genetic and pattern phases.
struct EvoSearch<'a> {
    ev: &'a mut Evaluator,
    rows: Vec<Row>,
    senses: Vec<Sense>,
    goal: Goal,
    lo: Vec<f64>,
    hi: Vec<f64>,
    integer: &'a [bool],
    tol: f64,
}

impl EvoSearch<'_> {
    /// Clamps into the box and rounds integer variables.
    fn repair(&self, x: &mut [f64]) {
        for (j, v) in x.iter_mut().enumerate() {
            let (l, h) = (self.lo.get(j).copied().unwrap_or(0.0), self.hi.get(j).copied().unwrap_or(0.0));
            *v = if v.is_finite() { v.clamp(l, h) } else { l };
            if self.integer.get(j).copied().unwrap_or(false) {
                *v = v.round().clamp(l.ceil(), h.floor());
            }
        }
    }

    /// Scores `x`; `None` once the budget is spent.
    fn score(&mut self, x: Vec<f64>) -> Option<Member> {
        let p = self.ev.eval(&x)?;
        let (phi, cons) = shape(&p, &self.rows, self.goal);
        let viol = violation(&cons, &self.senses);
        let phi = if phi.is_finite() { phi } else { f64::INFINITY };
        let viol = if viol.is_finite() && cons.iter().all(|c| c.is_finite()) { viol } else { f64::INFINITY };
        Some(Member { x, phi, viol })
    }
}

/// The Evolutionary method within `lo..hi` from `x0`.
fn evolutionary(ev: &mut Evaluator, x0: &[f64], lo: &[f64], hi: &[f64], integer: &[bool], goal: Goal, o: &Options) -> (Outcome, EvoEnd) {
    let rows = ev.rows.clone();
    let senses: Vec<Sense> = rows.iter().map(|r| r.sense).collect();
    let scale = rows.iter().map(|r| r.rhs_const.abs()).fold(1.0, f64::max);
    let mut start = x0.to_vec();
    project(&mut start, lo, hi);
    // Without a bound, search within 100 of the start.
    let blo: Vec<f64> = lo.iter().zip(&start).map(|(l, s)| if l.is_finite() { *l } else { s - 100.0 }).collect();
    let bhi: Vec<f64> = hi.iter().zip(&start).map(|(h, s)| if h.is_finite() { *h } else { s + 100.0 }).collect();
    let no_integer = blo.iter().zip(&bhi).zip(integer).any(|((l, h), int)| *int && l.ceil() > h.floor());
    if blo.iter().zip(&bhi).any(|(l, h)| l > h) || no_integer {
        return (Outcome::Infeasible, EvoEnd::Converged);
    }
    let span: Vec<f64> = blo.iter().zip(&bhi).map(|(l, h)| (h - l).max(1e-9)).collect();
    let mut se = EvoSearch { ev, rows, senses, goal, lo: blo, hi: bhi, integer, tol: o.precision * scale };
    let mut rng = Rng::new(o.seed);
    let size = o.population.clamp(MIN_POPULATION, MAX_POPULATION);
    let tol = se.tol;
    let mut end = EvoEnd::Budget;
    // The first generation: the start and random points in the box.
    let mut pop: Vec<Member> = Vec::with_capacity(size);
    se.repair(&mut start);
    let mut next_x = Some(start);
    while pop.len() < size {
        let x = next_x.take().unwrap_or_else(|| {
            let mut x: Vec<f64> = se.lo.iter().zip(&se.hi).map(|(l, h)| rng.between(*l, *h)).collect();
            se.repair(&mut x);
            x
        });
        match se.score(x) {
            Some(m) => pop.push(m),
            None => break,
        }
    }
    let mut best: Option<Member> = None;
    for m in &pop {
        if best.as_ref().is_none_or(|b| m.better(b, tol)) {
            best = Some(m.clone());
        }
    }
    let mut last_improved = 0u64;
    let mut generation = 0u64;
    'search: while pop.len() == size {
        let Some(b) = best.clone() else { break };
        // Converged: 99% of the population is feasible and within Convergence of the best.
        let close = pop.iter().filter(|m| m.viol <= tol && (m.phi - b.phi).abs() <= o.convergence * b.phi.abs().max(1.0)).count();
        if b.viol <= tol && generation >= 5 && close * 100 >= size * 99 {
            end = EvoEnd::Converged;
            break;
        }
        if generation.saturating_sub(last_improved) >= o.stall {
            end = EvoEnd::Stalled;
            break;
        }
        // The next generation: the best survives, the rest are children of two tournaments.
        let mut next = Vec::with_capacity(size);
        next.push(b.clone());
        while next.len() < size {
            let mut parent = || {
                let (i, k) = (rng.below(size), rng.below(size));
                match (pop.get(i), pop.get(k)) {
                    (Some(p), Some(q)) => Some(if q.better(p, tol) { q.x.clone() } else { p.x.clone() }),
                    _ => None,
                }
            };
            let (Some(pa), Some(pb)) = (parent(), parent()) else { break 'search };
            let mut x: Vec<f64> = pa
                .iter()
                .zip(&pb)
                .zip(&span)
                .map(|((a, c), w)| {
                    let mut v = a + rng.between(-0.25, 1.25) * (c - a);
                    if rng.uniform() < o.mutation {
                        v += 0.1 * w * rng.normal();
                    }
                    v
                })
                .collect();
            se.repair(&mut x);
            match se.score(x) {
                Some(m) => next.push(m),
                None => break 'search,
            }
        }
        generation += 1;
        for m in &next {
            let Some(cur) = &best else { continue };
            if m.better(cur, tol) {
                let improved = cur.viol > tol || cur.phi - m.phi > 1e-9 * cur.phi.abs().max(1.0);
                best = Some(m.clone());
                if improved {
                    last_improved = generation;
                }
            }
        }
        pop = next;
    }
    // Polish: a compass search on the continuous variables around the best member.
    if let Some(mut b) = best.clone() {
        let mut step: Vec<f64> = span.iter().map(|w| 0.01 * w).collect();
        let n = b.x.len();
        'polish: for _ in 0..10_000 {
            if step.iter().zip(&span).zip(integer).all(|((s, w), int)| *int || *s <= 1e-9 * w.max(1.0)) {
                break;
            }
            let mut moved = false;
            for j in 0..n {
                if integer.get(j).copied().unwrap_or(false) {
                    continue;
                }
                for dir in [1.0, -1.0] {
                    let mut x = b.x.clone();
                    if let (Some(v), Some(s)) = (x.get_mut(j), step.get(j)) {
                        *v += dir * s;
                    }
                    se.repair(&mut x);
                    if x == b.x {
                        continue;
                    }
                    let Some(m) = se.score(x) else { break 'polish };
                    if m.better(&b, tol) {
                        b = m;
                        moved = true;
                        break;
                    }
                }
            }
            if !moved {
                for s in &mut step {
                    *s *= 0.5;
                }
            }
        }
        best = Some(b);
    }
    match best {
        Some(b) if b.viol <= tol => (Outcome::Solved(b.x, b.phi, end != EvoEnd::Budget), end),
        _ if end == EvoEnd::Budget => (Outcome::Limit, end),
        _ => (Outcome::Infeasible, end),
    }
}

/// What a solve ended with, and why Simplex LP, Evolutionary or a limit stopped it.
struct Solve {
    outcome: Outcome,
    not_linear: bool,
    evo_end: Option<EvoEnd>,
    stop: Option<Stop>,
    /// GRG Nonlinear ran every multistart point.
    multistart: bool,
}

/// The linear program of a linearised model (see [`linearize`]): costs to minimise and rows
/// `a·x (sense) b`, the model's rows first.
#[allow(clippy::type_complexity)]
fn lp_problem(goal: Goal, cf: &[f64], f_const: f64, lin: &[(Vec<f64>, f64)], rows: &[Row]) -> (Vec<f64>, Vec<(Vec<f64>, Sense, f64)>) {
    let mut out: Vec<(Vec<f64>, Sense, f64)> = lin.iter().zip(rows).map(|((a, k), r)| (a.clone(), r.sense, -k)).collect();
    let c = match goal {
        Goal::Max => cf.iter().map(|v| -v).collect(),
        Goal::Min => cf.to_vec(),
        Goal::Value(v) => {
            out.push((cf.to_vec(), Sense::Eq, v - f_const));
            vec![0.0; cf.len()]
        }
    };
    (c, out)
}

/// GRG Nonlinear from one starting point (branch and bound on top for integers).
#[allow(clippy::too_many_arguments)]
fn grg(ev: &mut Evaluator, m: &Model, goal: Goal, x0: &[f64], lo: &[f64], hi: &[f64], integer: &[bool]) -> (Outcome, Option<Stop>) {
    if integer.iter().any(|b| *b) {
        let mut relax = |l: &[f64], h: &[f64]| {
            let mut start = x0.to_vec();
            project(&mut start, l, h);
            nlp(ev, &start, l, h, goal, &m.opt)
        };
        branch_and_bound(lo, hi, integer, BnbLimits::new(MAX_NODES_NLP, &m.opt), &mut relax)
    } else {
        (nlp(ev, x0, lo, hi, goal, &m.opt), None)
    }
}

/// GRG Nonlinear › Use Multistart: GRG from the current values and from random points in the
/// bounds (within 100 of the start where a variable has none), sharing the evaluation budget
/// evenly; the best solution wins. Returns whether every start ran.
#[allow(clippy::too_many_arguments)]
fn multistart(ev: &mut Evaluator, m: &Model, goal: Goal, x0: &[f64], lo: &[f64], hi: &[f64], integer: &[bool]) -> (Outcome, Option<Stop>, bool) {
    let starts = m.opt.population.clamp(MIN_POPULATION, MAX_POPULATION);
    let blo: Vec<f64> = lo.iter().zip(x0).map(|(l, s)| if l.is_finite() { *l } else { s - 100.0 }).collect();
    let bhi: Vec<f64> = hi.iter().zip(x0).map(|(h, s)| if h.is_finite() { *h } else { s + 100.0 }).collect();
    let mut rng = Rng::new(m.opt.seed);
    let budget = ev.budget;
    let mut best: Option<(Outcome, Option<Stop>)> = None;
    let mut fallback: Option<(Outcome, Option<Stop>)> = None;
    let mut all = true;
    for i in 0..starts {
        let left = budget.saturating_sub(ev.evals);
        if left == 0 {
            all = false;
            break;
        }
        let start: Vec<f64> = if i == 0 { x0.to_vec() } else { blo.iter().zip(&bhi).map(|(l, h)| rng.between(*l, *h)).collect() };
        ev.budget = ev.evals.saturating_add(left / (starts - i) as u64).min(budget);
        let (out, stop) = grg(ev, m, goal, &start, lo, hi, integer);
        match (&out, &best) {
            (Outcome::Solved(_, f, _), Some((Outcome::Solved(_, b, _), _))) if *f >= *b => {}
            (Outcome::Solved(..), _) => best = Some((out, stop)),
            // An unbounded start settles it.
            (Outcome::Unbounded, _) => {
                ev.budget = budget;
                return (Outcome::Unbounded, None, true);
            }
            _ => {
                if fallback.is_none() {
                    fallback = Some((out, stop));
                }
            }
        }
    }
    ev.budget = budget;
    let (out, stop) = best.or(fallback).unwrap_or((Outcome::Limit, Some(Stop::Evaluations)));
    (out, stop, all)
}

/// Runs the model's method for `goal` within `lo..hi` from `x0`.
#[allow(clippy::too_many_arguments)]
fn optimise(ev: &mut Evaluator, m: &Model, goal: Goal, x0: &[f64], lo: &[f64], hi: &[f64], integer: &[bool]) -> Solve {
    let any_int = integer.iter().any(|b| *b);
    let mut solve = Solve { outcome: Outcome::Infeasible, not_linear: false, evo_end: None, stop: None, multistart: false };
    if lo.iter().zip(hi).any(|(l, h)| l > h) {
        return solve;
    }
    match m.method {
        Method::Simplex => match linearize(ev, x0) {
            Err("nonlinear") | Err("error") => solve.not_linear = true,
            Err(_) => solve.outcome = Outcome::Limit,
            Ok(((cf, f_const), lin)) => {
                let (c, rows) = lp_problem(goal, &cf, f_const, &lin, &ev.rows);
                let size = (rows.len() as u64 + 2 * x0.len() as u64 + 1) * (4 * x0.len() as u64 + 2 * rows.len() as u64 + 1);
                let mut work = LP_WORK.max(size);
                let mut relax = |l: &[f64], h: &[f64]| solve_lp(&c, &rows, l, h, &mut work, None);
                if any_int {
                    (solve.outcome, solve.stop) = branch_and_bound(lo, hi, integer, BnbLimits::new(MAX_NODES_LP, &m.opt), &mut relax);
                } else {
                    solve.outcome = relax(lo, hi);
                }
            }
        },
        Method::Grg if m.opt.multistart => {
            let (out, stop, all) = multistart(ev, m, goal, x0, lo, hi, integer);
            (solve.outcome, solve.stop, solve.multistart) = (out, stop, all);
        }
        Method::Grg => (solve.outcome, solve.stop) = grg(ev, m, goal, x0, lo, hi, integer),
        Method::Evolutionary => {
            let (out, end) = evolutionary(ev, x0, lo, hi, integer, goal, &m.opt);
            solve.outcome = out;
            solve.evo_end = Some(end);
        }
    }
    solve
}

// ---------------------------------------------------------------- reports

#[derive(Clone, Copy, Debug, PartialEq)]
enum ReportKind {
    Answer,
    Sensitivity,
    Limits,
}

impl ReportKind {
    fn title(self) -> &'static str {
        match self {
            ReportKind::Answer => "Answer",
            ReportKind::Sensitivity => "Sensitivity",
            ReportKind::Limits => "Limits",
        }
    }
}

/// The `reports` list (`None` when absent).
fn report_kinds(cmd: &'static str, p: &Json) -> Result<Option<Vec<ReportKind>>> {
    let Some(list) = p.get("reports") else { return Ok(None) };
    let list = list.as_array().ok_or_else(|| bad(cmd, "`reports` must be an array of answer, sensitivity and limits"))?;
    let mut kinds = Vec::new();
    for k in list {
        let kind = match k.as_str().map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            Some("answer") => ReportKind::Answer,
            Some("sensitivity") => ReportKind::Sensitivity,
            Some("limits") => ReportKind::Limits,
            _ => return Err(bad(cmd, format!("unknown report {k} (answer, sensitivity or limits)"))),
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    Ok(Some(kinds))
}

/// A report's cells: (row, column, value, bold).
type Lines = Vec<(u32, u32, Value, bool)>;

fn abs_ref(c: CellRef) -> String {
    format!("${}${}", gridcraft_core::col_to_letters(c.col), u64::from(c.row) + 1)
}

/// A cell's name in the reports: the nearest text to its left and the nearest text above it.
fn cell_name(sh: &Sheet, c: CellRef) -> String {
    let text = |c: CellRef| match sh.value(c) {
        Value::Text(t) if !t.trim().is_empty() => Some(t.to_string()),
        _ => None,
    };
    let left = (0..c.col).rev().find_map(|col| text(CellRef::new(c.row, col)));
    let top = (0..c.row).rev().find_map(|row| text(CellRef::new(row, c.col)));
    [left, top].into_iter().flatten().collect::<Vec<_>>().join(" ")
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Simplex => "Simplex LP",
        Method::Grg => "GRG Nonlinear",
        Method::Evolutionary => "Evolutionary",
    }
}

/// Header labels in bold from column 1, then the rows below them; returns the next free row.
fn table(out: &mut Lines, row: u32, headers: &[&str], rows: Vec<Vec<Value>>) -> u32 {
    for (k, h) in headers.iter().enumerate() {
        out.push((row, 1 + k as u32, Value::text(*h), true));
    }
    let mut r = row;
    for cells in rows {
        r = r.saturating_add(1);
        for (k, v) in cells.into_iter().enumerate() {
            out.push((r, 1 + k as u32, v, false));
        }
    }
    r.saturating_add(2)
}

/// Everything a report describes: the model, its starting and final values, and the result.
struct ReportInput<'a> {
    model: &'a Model,
    prob: &'a Problem,
    original: Vec<f64>,
    solution: Vec<f64>,
    message: String,
    budget: u64,
}

fn title(out: &mut Lines, kind: ReportKind, sheet: &str) -> u32 {
    out.push((0, 0, Value::text(format!("GridCraft {} Report", kind.title())), true));
    out.push((1, 0, Value::text(format!("Worksheet: {sheet}")), false));
    3
}

/// The values of `cells` with the variable cells set to `x`.
fn read_at(trial: &mut Trial, m: &Model, x: &[f64], cells: &[CellRef]) -> Vec<f64> {
    let sets: Vec<(usize, CellRef, Value)> = m.vars.iter().zip(x).map(|(c, v)| (m.sheet, *c, Value::number(*v))).collect();
    let reads: Vec<(usize, CellRef)> = cells.iter().map(|c| (m.sheet, *c)).collect();
    trial.run(&sets, &reads).iter().map(as_num).collect()
}

fn answer_report(d: &DocState, sh: &Sheet, inp: &ReportInput) -> Lines {
    let m = inp.model;
    let mut trial = Trial::new(d);
    let read = |trial: &mut Trial, x: &[f64], cells: &[CellRef]| read_at(trial, m, x, cells);
    let f0 = read(&mut trial, &inp.original, &[m.objective]).first().copied().unwrap_or(f64::NAN);
    // Every cell the constraints read, at the solution.
    let mut cells = vec![m.objective];
    for c in &m.cons {
        cells.extend(&c.lhs);
        if let Rhs::Cells(r) = &c.rhs {
            cells.extend(r);
        }
    }
    let vals = read(&mut trial, &inp.solution, &cells);
    let mut at = vals.iter().copied();
    let f1 = at.next().unwrap_or(f64::NAN);
    let num = |v: f64| if v.is_finite() { Value::number(v) } else { Value::Error(gridcraft_core::CellError::Value) };
    let mut out = Lines::new();
    let mut r = title(&mut out, ReportKind::Answer, &sh.name);
    out.push((r, 0, Value::text(format!("Result: {}", inp.message)), false));
    out.push((r + 1, 0, Value::text(format!("Engine: {}", method_name(m.method))), false));
    r += 3;
    let goal = match m.goal {
        Goal::Max => "Max".to_string(),
        Goal::Min => "Min".to_string(),
        Goal::Value(v) => format!("Value Of {v}"),
    };
    out.push((r, 0, Value::text(format!("Objective Cell ({goal})")), true));
    let obj = vec![vec![Value::text(abs_ref(m.objective)), Value::text(cell_name(sh, m.objective)), num(f0), num(f1)]];
    r = table(&mut out, r + 1, &["Cell", "Name", "Original Value", "Final Value"], obj);
    out.push((r, 0, Value::text("Variable Cells"), true));
    let kind = |j: usize| {
        let bin = m.cons.iter().any(|c| c.rel == Rel::Bin && m.vars.get(j).is_some_and(|v| c.lhs.contains(v)));
        match (bin, inp.prob.integer.get(j).copied().unwrap_or(false)) {
            (true, _) => "Binary",
            (false, true) => "Integer",
            _ => "Contin",
        }
    };
    let vars = m
        .vars
        .iter()
        .enumerate()
        .map(|(j, c)| {
            let (a, b) = (inp.original.get(j).copied().unwrap_or(f64::NAN), inp.solution.get(j).copied().unwrap_or(f64::NAN));
            vec![Value::text(abs_ref(*c)), Value::text(cell_name(sh, *c)), num(a), num(b), Value::text(kind(j))]
        })
        .collect();
    r = table(&mut out, r + 1, &["Cell", "Name", "Original Value", "Final Value", "Integer"], vars);
    out.push((r, 0, Value::text("Constraints"), true));
    let mut rows = Vec::new();
    for c in &m.cons {
        let lhs: Vec<f64> = c.lhs.iter().map(|_| at.next().unwrap_or(f64::NAN)).collect();
        let rhs: Vec<f64> = match &c.rhs {
            Rhs::Cells(cells) => cells.iter().map(|_| at.next().unwrap_or(f64::NAN)).collect(),
            Rhs::Const(v) => vec![*v],
            Rhs::None => vec![],
        };
        for (k, (cell, v)) in c.lhs.iter().zip(&lhs).enumerate() {
            let (formula, status, slack) = match (c.rel, &c.rhs) {
                (Rel::Int, _) => (format!("{}=Integer", abs_ref(*cell)), "Binding", 0.0),
                (Rel::Bin, _) => (format!("{}=Binary", abs_ref(*cell)), "Binding", 0.0),
                (rel, rhs_cells) => {
                    let b = rhs.get(k).or_else(|| rhs.first()).copied().unwrap_or(f64::NAN);
                    let rhs_text = match rhs_cells {
                        Rhs::Cells(cells) => cells.get(k).or_else(|| cells.first()).map(|c| abs_ref(*c)).unwrap_or_default(),
                        _ => gridcraft_core::number_to_text(b),
                    };
                    let slack = (v - b).abs();
                    let binding = rel == Rel::Eq || slack <= m.opt.precision * (1.0 + b.abs());
                    let slack = if binding { 0.0 } else { slack };
                    (format!("{}{}{rhs_text}", abs_ref(*cell), rel.text()), if binding { "Binding" } else { "Not Binding" }, slack)
                }
            };
            rows.push(vec![
                Value::text(abs_ref(*cell)),
                Value::text(cell_name(sh, *cell)),
                num(*v),
                Value::text(formula),
                Value::text(status),
                num(slack),
            ]);
        }
    }
    table(&mut out, r + 1, &["Cell", "Name", "Cell Value", "Formula", "Status", "Slack"], rows);
    out
}

/// What the Sensitivity report shows for "no limit", as Excel does.
const NO_LIMIT: f64 = 1e30;

/// `v`, with rounding noise around zero cleared.
fn snap(v: f64, scale: f64) -> f64 {
    if v.abs() <= 1e-9 * scale.abs().max(1.0) { 0.0 } else { v }
}

/// Solves `a·x = b` (square) by Gaussian elimination with partial pivoting; a pivot that
/// vanishes leaves its unknown at 0.
fn solve_dense(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let k = b.len();
    let big = a.iter().flatten().fold(0.0f64, |m, v| m.max(v.abs()));
    let mut x = vec![0.0; k];
    let mut pivots = vec![None; k];
    let mut row = 0;
    for col in 0..k {
        let Some(p) = (row..k).max_by(|&i, &j| {
            let (u, v) = (a.get(i).and_then(|r| r.get(col)), a.get(j).and_then(|r| r.get(col)));
            u.map_or(0.0, |u| u.abs()).total_cmp(&v.map_or(0.0, |v| v.abs()))
        }) else {
            break;
        };
        if a.get(p).and_then(|r| r.get(col)).is_none_or(|v| v.abs() <= 1e-12 * big.max(1e-300)) {
            continue;
        }
        a.swap(row, p);
        b.swap(row, p);
        let (Some(prow), Some(pb)) = (a.get(row).cloned(), b.get(row).copied()) else { break };
        let pv = prow.get(col).copied().unwrap_or(1.0);
        for i in 0..k {
            if i == row {
                continue;
            }
            let f = a.get(i).and_then(|r| r.get(col)).copied().unwrap_or(0.0) / pv;
            if f != 0.0 {
                if let Some(r) = a.get_mut(i) {
                    for (v, q) in r.iter_mut().zip(&prow) {
                        *v -= f * q;
                    }
                }
                if let Some(v) = b.get_mut(i) {
                    *v -= f * pb;
                }
            }
        }
        if let Some(e) = pivots.get_mut(col) {
            *e = Some(row);
        }
        row += 1;
    }
    for (col, p) in pivots.iter().enumerate() {
        if let (Some(r), Some(e)) = (p, x.get_mut(col)) {
            let pv = a.get(*r).and_then(|q| q.get(col)).copied().unwrap_or(1.0);
            *e = b.get(*r).copied().unwrap_or(0.0) / pv;
        }
    }
    x
}

/// Simplex LP's sensitivity, from the final tableau: per variable the reduced cost, objective
/// coefficient and how far it may rise or fall with the same basis; per constraint row the
/// shadow price and how far its right side may rise or fall. Values are in the objective cell's
/// units (a Max model's tableau minimises −objective).
#[allow(clippy::type_complexity)]
fn lp_sensitivity(fin: &LpFinal, goal: Goal, cf: &[f64], lin: &[(Vec<f64>, f64)], nrows: usize) -> (Vec<[f64; 4]>, Vec<[f64; 3]>) {
    let s = if goal == Goal::Max { -1.0 } else { 1.0 };
    let at = |i: usize, j: usize| fin.t.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0);
    let cost = |j: usize| fin.obj.get(j).copied().unwrap_or(0.0);
    let scale = cf.iter().fold(1.0f64, |m, v| m.max(v.abs()));
    // Shadow prices: a row's dual is minus its unit column's reduced cost over the column's
    // coefficient there; the row sign undoes the tableau's normalisation.
    let shadow: Vec<f64> = (0..nrows)
        .map(|i| {
            let (col, coef) = fin.unit.get(i).copied().unwrap_or((0, 1.0));
            let pi = -cost(col) / coef;
            snap(s * pi * fin.row_sign.get(i).copied().unwrap_or(1.0), scale)
        })
        .collect();
    let in_basis = |l: usize| fin.basis.contains(&l);
    let mut vars = Vec::with_capacity(cf.len());
    for (j, c) in cf.iter().enumerate() {
        let reduced = c - shadow.iter().zip(lin).map(|(y, (a, _))| y * a.get(j).copied().unwrap_or(0.0)).sum::<f64>();
        // A change δ in the variable's cost moves each nonbasic reduced cost d_l by δ·e_l; the
        // basis stays optimal while every d_l stays ≥ 0.
        let dcost: Vec<f64> = (0..fin.width).map(|k| fin.cols.get(k).filter(|(v, _)| *v == j).map_or(0.0, |(_, sign)| *sign)).collect();
        let (mut up, mut down) = (NO_LIMIT, NO_LIMIT);
        for l in (0..fin.art_from).filter(|l| !in_basis(*l)) {
            let e = dcost.get(l).copied().unwrap_or(0.0)
                - fin.basis.iter().enumerate().map(|(i, b)| dcost.get(*b).copied().unwrap_or(0.0) * at(i, l)).sum::<f64>();
            let d = cost(l).max(0.0);
            if e < -EPS {
                up = up.min(d / -e);
            } else if e > EPS {
                down = down.min(d / e);
            }
        }
        // The tableau's cost is s·coefficient: for Max, a rise in the coefficient is a fall in it.
        let (up, down) = if s < 0.0 { (down, up) } else { (up, down) };
        vars.push([snap(reduced, scale), *c, snap(up, scale), snap(down, scale)]);
    }
    let mut cons = Vec::with_capacity(nrows);
    for (r, y) in shadow.iter().enumerate() {
        // A change Δ in the right side moves the basic values by Δ·sign·B⁻¹eᵣ; they must stay ≥ 0
        // (and artificials left in the basis at 0).
        let (col, coef) = fin.unit.get(r).copied().unwrap_or((0, 1.0));
        let sg = fin.row_sign.get(r).copied().unwrap_or(1.0);
        let (mut up, mut down) = (NO_LIMIT, NO_LIMIT);
        for (i, b) in fin.basis.iter().enumerate() {
            let w = sg * at(i, col) / coef;
            let v = at(i, fin.width).max(0.0);
            if *b >= fin.art_from {
                if w.abs() > EPS {
                    (up, down) = (0.0, 0.0);
                }
                continue;
            }
            if w < -EPS {
                up = up.min(v / -w);
            } else if w > EPS {
                down = down.min(v / w);
            }
        }
        cons.push([*y, snap(up, 1.0), snap(down, 1.0)]);
    }
    (vars, cons)
}

/// GRG Nonlinear's sensitivity at a local optimum: the multipliers of the binding constraints
/// (least squares on the variables not at a bound, from central-difference gradients) and each
/// variable's reduced gradient. A multiplier is the objective's rate of change as the constraint's
/// right side rises. GRG stops a little inside its constraints, so rows within 0.01% of their
/// limit may bind; one whose multiplier has the wrong sign for a binding row is let go.
fn nlp_sensitivity(ev: &mut Evaluator, m: &Model, prob: &Problem, x: &[f64], sides: &[(f64, f64)]) -> Result<(Vec<f64>, Vec<f64>)> {
    let n = x.len();
    let rows = ev.rows.clone();
    let tol = m.opt.precision;
    let out_of_budget = || EngineError::Other("Solver ran out of evaluations writing the Sensitivity report.".into());
    let p0 = ev.eval(x).ok_or_else(out_of_budget)?;
    let mut gf = vec![0.0; n];
    let mut gg = vec![vec![0.0; n]; rows.len()];
    for j in 0..n {
        let xj = x.get(j).copied().unwrap_or(0.0);
        let h = 1e-6 * xj.abs().max(1.0);
        let (l, u) = (prob.lo.get(j).copied().unwrap_or(f64::NEG_INFINITY), prob.hi.get(j).copied().unwrap_or(f64::INFINITY));
        let (a, b) = (if xj - h >= l { xj - h } else { xj }, if xj + h <= u { xj + h } else { xj });
        if b <= a {
            continue;
        }
        let mut point = |t: f64| -> Result<Point> {
            if t == xj {
                return Ok(p0.clone());
            }
            let mut xt = x.to_vec();
            if let Some(v) = xt.get_mut(j) {
                *v = t;
            }
            ev.eval(&xt).ok_or_else(out_of_budget)
        };
        let (pa, pb) = (point(a)?, point(b)?);
        if let Some(v) = gf.get_mut(j) {
            *v = (pb.f - pa.f) / (b - a);
        }
        for (row, (gb, ga)) in gg.iter_mut().zip(pb.g.iter().zip(&pa.g)) {
            if let Some(v) = row.get_mut(j) {
                *v = (gb - ga) / (b - a);
            }
        }
    }
    // Rows that may bind, and the variables not held at a bound.
    let mut active: Vec<usize> = rows
        .iter()
        .zip(&p0.g)
        .zip(sides)
        .enumerate()
        .filter(|(_, ((r, g), (l, rv)))| {
            let t = (10.0 * tol).max(1e-4) * l.abs().max(rv.abs()).max(1.0);
            match r.sense {
                Sense::Eq => true,
                Sense::Le => **g >= -t,
                Sense::Ge => **g <= t,
            }
        })
        .map(|(i, _)| i)
        .collect();
    let free: Vec<usize> = (0..n)
        .filter(|&j| {
            let v = x.get(j).copied().unwrap_or(0.0);
            let near = |b: Option<&f64>| b.is_some_and(|b| b.is_finite() && (v - b).abs() <= tol * (1.0 + b.abs()));
            !near(prob.lo.get(j)) && !near(prob.hi.get(j))
        })
        .collect();
    let row = |r: usize, j: usize| gg.get(r).and_then(|g| g.get(j)).copied().unwrap_or(0.0);
    let scale = gf.iter().fold(1.0f64, |m, v| m.max(v.abs()));
    // The sign a binding row's multiplier must have: raising a ≤ row's right side can only help
    // (raise a Max objective, lower a Min one), raising a ≥ row's can only hurt.
    let wrong = |r: usize, l: f64| {
        let helps = match m.goal {
            Goal::Max => 1.0,
            Goal::Min => -1.0,
            Goal::Value(_) => return false,
        };
        let sign = match rows.get(r).map(|x| x.sense) {
            Some(Sense::Le) => helps,
            Some(Sense::Ge) => -helps,
            _ => return false,
        };
        l * sign < -1e-6 * scale
    };
    let lam = loop {
        let mat: Vec<Vec<f64>> =
            active.iter().map(|&r| active.iter().map(|&q| free.iter().map(|&j| row(r, j) * row(q, j)).sum()).collect()).collect();
        let rhs: Vec<f64> = active.iter().map(|&r| free.iter().map(|&j| row(r, j) * gf.get(j).copied().unwrap_or(0.0)).sum()).collect();
        let lam = solve_dense(mat, rhs);
        match active.iter().zip(&lam).position(|(r, l)| wrong(*r, *l)) {
            Some(k) => {
                active.remove(k);
            }
            None => break lam,
        }
    };
    let mut multipliers = vec![0.0; rows.len()];
    for (r, l) in active.iter().zip(&lam) {
        if let Some(v) = multipliers.get_mut(*r) {
            *v = snap(*l, scale);
        }
    }
    let reduced = (0..n)
        .map(|j| {
            let v = gf.get(j).copied().unwrap_or(0.0) - multipliers.iter().enumerate().map(|(r, l)| l * row(r, j)).sum::<f64>();
            // Finite differences leave noise where the gradient is balanced.
            if free.contains(&j) && v.abs() <= 1e-5 * scale { 0.0 } else { snap(v, scale) }
        })
        .collect();
    Ok((reduced, multipliers))
}

fn sensitivity_report(d: &DocState, sh: &Sheet, inp: &ReportInput) -> Result<Lines> {
    let m = inp.model;
    if inp.prob.integer.iter().any(|b| *b) {
        return Err(EngineError::Other("The Sensitivity report doesn't apply to models with integer or binary constraints.".into()));
    }
    if m.method == Method::Evolutionary {
        return Err(EngineError::Other("The Sensitivity report is available for Simplex LP and GRG Nonlinear.".into()));
    }
    let x = &inp.solution;
    let n = x.len();
    let mut ev = Evaluator {
        trial: Trial::new(d),
        sheet: m.sheet,
        vars: m.vars.clone(),
        objective: m.objective,
        rows: inp.prob.rows.clone(),
        evals: 0,
        budget: (2 * n as u64 + 4).max(inp.budget),
        target: None,
    };
    let rows = inp.prob.rows.clone();
    // Each row's left and right side at the solution.
    let mut cells = Vec::new();
    for r in &rows {
        cells.push(r.lhs);
        cells.extend(r.rhs);
    }
    let vals = read_at(&mut Trial::new(d), m, x, &cells);
    let mut it = vals.iter().copied();
    let sides: Vec<(f64, f64)> =
        rows.iter().map(|r| (it.next().unwrap_or(f64::NAN), if r.rhs.is_some() { it.next().unwrap_or(f64::NAN) } else { r.rhs_const })).collect();
    let num = |v: f64| if v.is_finite() { Value::number(v) } else { Value::Error(gridcraft_core::CellError::Value) };
    let mut out = Lines::new();
    let mut r = title(&mut out, ReportKind::Sensitivity, &sh.name);
    out.push((r, 0, Value::text(format!("Engine: {}", method_name(m.method))), false));
    r += 2;
    let var_rows: Vec<Vec<Value>>;
    let con_rows: Vec<Vec<Value>>;
    let (var_heads, con_heads): (&[&str], &[&str]);
    if m.method == Method::Simplex {
        let ((cf, f_const), lin) = linearize(&mut ev, x).map_err(|e| {
            EngineError::Other(match e {
                "budget" => "Solver ran out of evaluations writing the Sensitivity report.".into(),
                _ => "The model isn't linear in the variable cells, so Simplex LP has no Sensitivity report. Try GRG Nonlinear.".to_string(),
            })
        })?;
        let (c, lp_rows) = lp_problem(m.goal, &cf, f_const, &lin, &rows);
        let mut fin = None;
        let mut work = LP_WORK;
        let solved = matches!(solve_lp(&c, &lp_rows, &inp.prob.lo, &inp.prob.hi, &mut work, Some(&mut fin)), Outcome::Solved(..));
        let fin = fin
            .filter(|_| solved)
            .ok_or_else(|| EngineError::Other("The model has no optimal solution, so there is no Sensitivity report.".into()))?;
        let (vs, cs) = lp_sensitivity(&fin, m.goal, &cf, &lin, rows.len());
        var_heads = &["Cell", "Name", "Final Value", "Reduced Cost", "Objective Coefficient", "Allowable Increase", "Allowable Decrease"];
        con_heads = &["Cell", "Name", "Final Value", "Shadow Price", "Constraint R.H. Side", "Allowable Increase", "Allowable Decrease"];
        var_rows = m
            .vars
            .iter()
            .zip(vs)
            .zip(x)
            .map(|((c, v), xv)| {
                let mut row = vec![Value::text(abs_ref(*c)), Value::text(cell_name(sh, *c)), num(*xv)];
                row.extend(v.map(num));
                row
            })
            .collect();
        con_rows = rows
            .iter()
            .zip(cs)
            .zip(&sides)
            .map(|((row, [y, up, down]), (l, rv))| {
                vec![Value::text(abs_ref(row.lhs)), Value::text(cell_name(sh, row.lhs)), num(*l), num(y), num(*rv), num(up), num(down)]
            })
            .collect();
    } else {
        let (reduced, multipliers) = nlp_sensitivity(&mut ev, m, inp.prob, x, &sides)?;
        var_heads = &["Cell", "Name", "Final Value", "Reduced Gradient"];
        con_heads = &["Cell", "Name", "Final Value", "Lagrange Multiplier"];
        var_rows = m
            .vars
            .iter()
            .zip(&reduced)
            .zip(x)
            .map(|((c, g), xv)| vec![Value::text(abs_ref(*c)), Value::text(cell_name(sh, *c)), num(*xv), num(*g)])
            .collect();
        con_rows = rows
            .iter()
            .zip(&multipliers)
            .zip(&sides)
            .map(|((row, l), (v, _))| vec![Value::text(abs_ref(row.lhs)), Value::text(cell_name(sh, row.lhs)), num(*v), num(*l)])
            .collect();
    }
    out.push((r, 0, Value::text("Variable Cells"), true));
    r = table(&mut out, r + 1, var_heads, var_rows);
    out.push((r, 0, Value::text("Constraints"), true));
    table(&mut out, r + 1, con_heads, con_rows);
    Ok(out)
}

fn limits_report(d: &DocState, sh: &Sheet, inp: &ReportInput) -> Result<Lines> {
    let m = inp.model;
    if inp.prob.integer.iter().any(|b| *b) {
        return Err(EngineError::Other("The Limits report doesn't apply to models with integer or binary constraints.".into()));
    }
    if m.method == Method::Evolutionary {
        return Err(EngineError::Other("The Limits report is available for Simplex LP and GRG Nonlinear.".into()));
    }
    let n = m.vars.len();
    let x = &inp.solution;
    // One local solve per limit: no multistart.
    let mut single = m.clone();
    single.opt.multistart = false;
    let m = &single;
    // Each variable is minimised and maximised with the others held at the solution.
    let per = (inp.budget / (2 * n as u64).max(1)).max(n as u64 + 3);
    let mut ev = Evaluator {
        trial: Trial::new(d),
        sheet: m.sheet,
        vars: m.vars.clone(),
        objective: m.objective,
        rows: inp.prob.rows.clone(),
        evals: 0,
        budget: 0,
        target: None,
    };
    let objective_at = |ev: &mut Evaluator, x: &[f64]| {
        ev.target = None;
        ev.budget = ev.evals.saturating_add(1);
        ev.eval(x).map(|p| p.f).unwrap_or(f64::NAN)
    };
    let num = |v: f64| if v.is_finite() { Value::number(v) } else { Value::Error(gridcraft_core::CellError::Value) };
    let f = objective_at(&mut ev, x);
    let mut rows = Vec::new();
    for (j, c) in m.vars.iter().enumerate() {
        let (mut lo, mut hi) = (x.clone(), x.clone());
        if let (Some(l), Some(h), Some(pl), Some(ph)) = (lo.get_mut(j), hi.get_mut(j), inp.prob.lo.get(j), inp.prob.hi.get(j)) {
            *l = *pl;
            *h = *ph;
        }
        let mut ends = Vec::new();
        for goal in [Goal::Min, Goal::Max] {
            ev.target = Some(j);
            ev.evals = 0;
            ev.budget = per;
            let solve = optimise(&mut ev, m, goal, x, &lo, &hi, &inp.prob.integer);
            ends.push(match solve.outcome {
                Outcome::Solved(p, _, _) => {
                    let v = p.get(j).copied().unwrap_or(f64::NAN);
                    (num(v), num(objective_at(&mut ev, &p)))
                }
                Outcome::Unbounded => (Value::text("Unbounded"), Value::Empty),
                _ => (Value::Error(gridcraft_core::CellError::NA), Value::Empty),
            });
        }
        let mut row = vec![Value::text(abs_ref(*c)), Value::text(cell_name(sh, *c)), num(x.get(j).copied().unwrap_or(f64::NAN))];
        for (v, o) in ends {
            row.push(v);
            row.push(o);
        }
        rows.push(row);
    }
    let mut out = Lines::new();
    let r = title(&mut out, ReportKind::Limits, &sh.name);
    let obj = vec![vec![Value::text(abs_ref(m.objective)), Value::text(cell_name(sh, m.objective)), num(f)]];
    let r = table(&mut out, r, &["Objective Cell", "Name", "Value"], obj);
    table(&mut out, r, &["Variable Cell", "Name", "Value", "Lower Limit", "Objective Result", "Upper Limit", "Objective Result"], rows);
    Ok(out)
}

/// Writes the reports on new sheets ("Answer Report 1"…) before the model's sheet, in one step;
/// the model's sheet stays active. Returns the new sheets' names.
fn write_reports(s: &mut Session, kinds: &[ReportKind], inp: &ReportInput) -> Result<Vec<String>> {
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let d = s.doc()?;
    if d.wb.protected_structure {
        return Err(EngineError::Other("The workbook is protected and cannot be changed.".into()));
    }
    let sh = d.wb.sheet(inp.model.sheet).ok_or(EngineError::NoDocument)?;
    let mut built = Vec::new();
    for k in kinds {
        let lines = match k {
            ReportKind::Answer => answer_report(d, sh, inp),
            ReportKind::Sensitivity => sensitivity_report(d, sh, inp)?,
            ReportKind::Limits => limits_report(d, sh, inp)?,
        };
        let name = (1..=10_000u32)
            .map(|i| format!("{} Report {i}", k.title()))
            .find(|n| d.wb.sheet_index(n).is_none() && !built.iter().any(|(b, _): &(String, Lines)| b == n))
            .ok_or_else(|| EngineError::Other("Too many report sheets.".into()))?;
        built.push((name, lines));
    }
    let names: Vec<String> = built.iter().map(|(n, _)| n.clone()).collect();
    let at = inp.model.sheet;
    let count = built.len();
    edit(s, |cx| {
        let bold = cx.wb.styles.derive(gridcraft_model::StyleId::DEFAULT, |st| st.font.bold = true);
        for (i, (name, lines)) in built.into_iter().enumerate() {
            let mut sheet = Sheet::new(name);
            for (r, c, v, b) in lines {
                let style = if b { bold } else { gridcraft_model::StyleId::DEFAULT };
                sheet.set_cell(CellRef::new(r, c), Cell { value: v, formula: None, style });
            }
            for (c, w) in [(0u32, 14.0f32), (1, 110.0), (2, 140.0), (3, 120.0), (4, 120.0), (5, 150.0), (6, 120.0), (7, 120.0)] {
                sheet.cols.insert(c, gridcraft_model::LineInfo { size: Some(w), ..Default::default() });
            }
            cx.wb.sheets.insert(at + i, std::sync::Arc::new(sheet));
        }
        for nm in cx.wb.names.iter_mut() {
            if let Some(sc) = nm.scope.as_mut()
                && *sc >= at
            {
                *sc += count;
            }
        }
        if cx.wb.active_sheet >= at {
            cx.wb.active_sheet += count;
        }
        cx.structural = true;
        Ok(())
    })?;
    if let Ok(d) = s.doc_mut() {
        for i in 0..count {
            d.sheet_selections.insert((at + i).min(d.sheet_selections.len()), crate::Selection::default());
        }
    }
    Ok(names)
}

/// `data.solverReport`: reports for a solution of the sheet's saved model.
fn report(s: &mut Session, p: &Json) -> Result<Json> {
    let kinds = report_kinds(REPORT_CMD, p)?.ok_or_else(|| bad(REPORT_CMD, "missing `reports` (answer, sensitivity, limits)"))?;
    if kinds.is_empty() {
        return Err(bad(REPORT_CMD, "`reports` is empty (answer, sensitivity, limits)"));
    }
    let d = s.doc()?;
    // The given model as is, else the saved model with any params given here over it.
    let params = match p.get("model") {
        Some(m @ Json::Object(_)) => m.clone(),
        Some(Json::Null) | None => {
            let mut params = saved_model(&d.wb, d.wb.active_sheet).unwrap_or_else(|| json!({}));
            if let (Some(base), Some(over)) = (params.as_object_mut(), p.as_object()) {
                for (k, v) in over {
                    base.insert(k.clone(), v.clone());
                }
            }
            params
        }
        Some(_) => return Err(bad(REPORT_CMD, "`model` must be data.solver params")),
    };
    let model = parse_model(s, &params)?;
    let prob = build_problem(&model);
    let d = s.doc()?;
    let sh = d.wb.sheet(model.sheet).ok_or(EngineError::NoDocument)?;
    let current: Vec<f64> = model.vars.iter().map(|c| as_num(&sh.value(*c))).collect();
    // `{cell: number}` maps; cells they leave out keep their current values.
    let point = |key: &str| -> Result<Vec<f64>> {
        let mut x = current.clone();
        let Some(map) = p.get(key) else { return Ok(x) };
        let map = map.as_object().ok_or_else(|| bad(REPORT_CMD, format!("`{key}` must map cells to numbers")))?;
        for (cell, v) in map {
            let c = CellRef::parse(&cell.replace('$', "")).ok_or_else(|| bad(REPORT_CMD, format!("not a cell: `{cell}`")))?;
            let j = model.vars.iter().position(|v| *v == c).ok_or_else(|| bad(REPORT_CMD, format!("`{cell}` is not a variable cell")))?;
            let v = v.as_f64().filter(|v| v.is_finite()).ok_or_else(|| bad(REPORT_CMD, format!("`{key}.{cell}` must be a number")))?;
            if let Some(e) = x.get_mut(j) {
                *e = v;
            }
        }
        Ok(x)
    };
    let solution = point("values")?;
    let original = if p.get("original").is_some() { point("original")? } else { solution.clone() };
    let budget = model.opt.max_evals;
    let message = str_param(p, "message").unwrap_or("").chars().take(500).collect();
    let inp = ReportInput { model: &model, prob: &prob, original, solution, message, budget };
    let sheets = write_reports(s, &kinds, &inp)?;
    Ok(json!({"sheets": sheets}))
}

// ---------------------------------------------------------------- command

fn run(s: &mut Session, p: &Json) -> Result<Json> {
    let d = s.doc()?;
    let active = d.wb.active_sheet;
    let mut params = p.clone();
    if bool_param(p, "saved").unwrap_or(false) {
        let saved = saved_model(&d.wb, active).ok_or_else(|| EngineError::Other("This sheet has no saved Solver model.".into()))?;
        if let (Some(base), Some(over)) = (saved.as_object(), p.as_object()) {
            let mut merged = base.clone();
            for (k, v) in over {
                merged.insert(k.clone(), v.clone());
            }
            params = Json::Object(merged);
        }
    }
    let has_objective = ["objective", "setObjective", "set"].iter().any(|k| str_param(&params, k).is_some_and(|t| !t.trim().is_empty()));
    if !has_objective {
        let model = saved_model(&d.wb, active);
        s.ui_requests.push(crate::UiRequest::Dialog("solver".into(), json!({})));
        return Ok(json!({"model": model}));
    }
    let model = parse_model(s, &params)?;
    let budget = model.opt.max_evals;
    let keep = bool_param(&params, "keep").unwrap_or(true);
    let save = bool_param(&params, "saveModel").unwrap_or(true);
    let reports = report_kinds(CMD, &params)?;
    let prob = build_problem(&model);
    let d = s.doc()?;
    let sh = d.wb.sheet(model.sheet).ok_or(EngineError::NoDocument)?;
    let x0: Vec<f64> = model.vars.iter().map(|c| as_num(&sh.value(*c))).map(|v| if v.is_finite() { v } else { 0.0 }).collect();
    let mut ev = Evaluator {
        trial: Trial::new(d),
        sheet: model.sheet,
        vars: model.vars.clone(),
        objective: model.objective,
        rows: prob.rows.clone(),
        evals: 0,
        budget,
        target: None,
    };
    let unbounded = prob.lo.iter().chain(&prob.hi).any(|b| !b.is_finite());
    if model.method == Method::Evolutionary && model.opt.require_bounds && unbounded {
        return Err(EngineError::Other(
            "The Evolutionary method needs a lower and an upper bound on every variable cell (constraints such as \"A1:A3 <= 10\"), or requireBounds: false."
                .into(),
        ));
    }
    if model.method == Method::Grg && model.opt.multistart && model.opt.require_bounds && unbounded {
        return Err(EngineError::Other(
            "Multistart needs a lower and an upper bound on every variable cell (constraints such as \"A1:A3 <= 10\"), or requireBounds: false."
                .into(),
        ));
    }
    let Solve { outcome, not_linear, evo_end, stop, multistart } = optimise(&mut ev, &model, model.goal, &x0, &prob.lo, &prob.hi, &prob.integer);
    let (status, message, solution) = match outcome {
        _ if not_linear => ("notLinear", "The model isn't linear in the variable cells, so Simplex LP can't solve it. Try GRG Nonlinear.", None),
        Outcome::Solved(x, _, false) if stop == Some(Stop::Subproblems) => {
            ("iterationLimit", "Solver stopped at the Max Subproblems limit; the best integer solution found satisfies the constraints.", Some(x))
        }
        Outcome::Solved(x, _, false) if stop == Some(Stop::FeasibleSolutions) => (
            "iterationLimit",
            "Solver stopped at the Max Feasible Solutions limit; the best integer solution found satisfies the constraints.",
            Some(x),
        ),
        Outcome::Limit if stop == Some(Stop::Subproblems) => {
            ("iterationLimit", "Solver stopped at the Max Subproblems limit without finding an integer solution.", None)
        }
        Outcome::Solved(x, _, true) if multistart => (
            "converged",
            "Multistart: Solver ran from every starting point and kept the best locally optimal solution. All constraints are satisfied.",
            Some(x),
        ),
        Outcome::Solved(x, _, true) if model.method == Method::Evolutionary => match evo_end {
            Some(EvoEnd::Stalled) => {
                ("cannotImprove", "Solver stopped when the best solution stopped improving. All constraints are satisfied.", Some(x))
            }
            _ => ("converged", "The population converged. The best solution found satisfies all constraints.", Some(x)),
        },
        Outcome::Solved(x, _, true) => (
            if model.method == Method::Simplex { "optimal" } else { "converged" },
            if model.method == Method::Simplex {
                "Solver found an optimal solution. All constraints are satisfied."
            } else {
                "Solver converged to a locally optimal solution. All constraints are satisfied."
            },
            Some(x),
        ),
        Outcome::Solved(x, _, false) => {
            ("iterationLimit", "Solver stopped at its evaluation limit; the best solution found satisfies the constraints.", Some(x))
        }
        Outcome::Limit => ("iterationLimit", "Solver stopped at its evaluation limit without finding a feasible solution.", None),
        Outcome::Infeasible => ("infeasible", "Solver could not find a solution that satisfies all constraints.", None),
        Outcome::Unbounded => ("unbounded", "The objective can grow without limit: the problem is unbounded.", None),
    };
    let evaluations = ev.evals;
    // The objective's value at the solution.
    let objective = solution.as_ref().and_then(|x| {
        ev.budget = ev.budget.saturating_add(1);
        ev.eval(x).map(|p| p.f)
    });
    let mut values = serde_json::Map::new();
    if let Some(x) = &solution {
        for (c, v) in model.vars.iter().zip(x) {
            values.insert(c.a1(), json!(v));
        }
    }
    let original: serde_json::Map<String, Json> = model.vars.iter().zip(&x0).map(|(c, v)| (c.a1(), json!(v))).collect();
    let changed = keep && solution.is_some();
    if changed || save {
        let sheet = model.sheet;
        let vars = model.vars.clone();
        let m = model.clone();
        edit(s, |cx| {
            if changed && let Some(x) = &solution {
                let sh = cx.sheet_mut(sheet)?;
                for (c, v) in vars.iter().zip(x) {
                    let style = sh.style_id(*c);
                    sh.set_cell(*c, Cell { value: Value::number(*v), formula: None, style });
                }
                for c in &vars {
                    cx.touch(sheet, *c);
                }
            }
            if save {
                save_model(&mut cx.wb, &m);
            }
            Ok(())
        })?;
    }
    let mut out = json!({
        "status": status,
        "message": message,
        "objective": objective,
        "values": values,
        "original": original,
        "model": model_params(&s.doc()?.wb, &model),
        "evaluations": evaluations,
        "changed": changed,
    });
    // Reports describe a solution; they are part of the solve's undo step.
    if let (Some(kinds), Some(x)) = (reports, solution) {
        let inp = ReportInput { model: &model, prob: &prob, original: x0, solution: x, message: message.to_string(), budget };
        out["reports"] = json!(write_reports(s, &kinds, &inp)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use gridcraft_core::{CellRef, Value};
    use serde_json::{Value as Json, json};

    use crate::Session;

    fn s() -> Session {
        let mut s = Session::new();
        s.new_workbook();
        s
    }
    fn n(s: &Session, a: &str) -> f64 {
        let d = s.doc().unwrap();
        d.wb.active().unwrap().value(CellRef::parse(a).unwrap()).as_f64().unwrap_or(f64::NAN)
    }
    /// The first saved-model name scoped to the active sheet.
    fn saved(s: &Session) -> String {
        let d = s.doc().unwrap();
        d.wb.names.iter().find(|n| n.scope == Some(d.wb.active_sheet)).map(|n| n.name.clone()).unwrap_or_default()
    }
    fn set(s: &mut Session, cells: &[(&str, &str)]) {
        for (c, v) in cells {
            s.execute("cell.set", json!({"cell": c, "input": v})).unwrap();
        }
    }

    #[test]
    fn simplex_solves_the_wyndor_glass_lp() {
        // Hillier & Lieberman's Wyndor Glass problem: max 3x + 5y, x ≤ 4, 2y ≤ 12, 3x + 2y ≤ 18
        // → x = 2, y = 6, Z = 36.
        let mut s = s();
        set(&mut s, &[("A1", "0"), ("A2", "0"), ("B1", "=3*A1+5*A2"), ("B2", "=2*A2"), ("B3", "=3*A1+2*A2")]);
        let r = s
            .execute(
                "data.solver",
                json!({"objective": "B1", "goal": "max", "variables": "A1:A2", "constraints": ["A1 <= 4", "B2 <= 12", {"lhs": "B3", "op": "<=", "rhs": 18}], "method": "simplex"}),
            )
            .unwrap();
        assert_eq!(r["status"], "optimal", "{r}");
        assert!((n(&s, "A1") - 2.0).abs() < 1e-9 && (n(&s, "A2") - 6.0).abs() < 1e-9);
        assert!((n(&s, "B1") - 36.0).abs() < 1e-9);
        // The model is saved as hidden names, survives XLSX and solves again from them.
        let names: Vec<String> = s.doc().unwrap().wb.names.iter().map(|n| n.name.clone()).collect();
        assert!(names.iter().any(|n| n == "solver_opt") && names.iter().any(|n| n == "solver_lhs3"));
        let b64 = s.execute("file.saveBytes", json!({"format": "xlsx"})).unwrap()["base64"].as_str().unwrap().to_string();
        s.execute("file.open", json!({"name": "m.xlsx", "base64": b64})).unwrap();
        set(&mut s, &[("A1", "0"), ("A2", "0")]);
        let m = s.execute("data.solver", json!({})).unwrap();
        assert_eq!(m["model"]["objective"], "B1");
        assert_eq!(m["model"]["method"], "simplex");
        let r = s.execute("data.solver", json!({"saved": true})).unwrap();
        assert_eq!(r["status"], "optimal");
        assert!((n(&s, "B1") - 36.0).abs() < 1e-9);
        // Minimise with keep: false leaves the cells alone.
        set(&mut s, &[("A1", "1"), ("A2", "1")]);
        let r = s.execute("data.solver", json!({"saved": true, "goal": "min", "keep": false})).unwrap();
        assert_eq!(r["objective"], 0.0);
        assert_eq!((n(&s, "A1"), n(&s, "A2")), (1.0, 1.0));
    }

    #[test]
    fn integer_and_binary_constraints_use_branch_and_bound() {
        // max 5x + 4y, 6x + 4y ≤ 24, x + 2y ≤ 6: the LP optimum (3, 1.5) = 21; integers give
        // (4, 0) = 20.
        let mut s = s();
        set(&mut s, &[("B1", "=5*A1+4*A2"), ("B2", "=6*A1+4*A2"), ("B3", "=A1+2*A2")]);
        let base = json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 24", "B3 <= 6"], "method": "simplex"});
        let r = s.execute("data.solver", base.clone()).unwrap();
        assert!((r["objective"].as_f64().unwrap() - 21.0).abs() < 1e-9);
        let mut p = base.clone();
        p["constraints"].as_array_mut().unwrap().push(json!("A1:A2 int"));
        let r = s.execute("data.solver", p).unwrap();
        assert_eq!(r["status"], "optimal");
        assert_eq!((n(&s, "A1"), n(&s, "A2"), n(&s, "B1")), (4.0, 0.0, 20.0));
        // 0/1 knapsack: values 10, 13, 7, weights 5, 6, 4, capacity 10 → items 2 and 3 (20).
        set(&mut s, &[("D1", "=10*C1+13*C2+7*C3"), ("D2", "=5*C1+6*C2+4*C3")]);
        let r = s
            .execute("data.solver", json!({"objective": "D1", "variables": "C1:C3", "constraints": ["D2 <= 10", "C1:C3 bin"], "method": "simplex"}))
            .unwrap();
        assert_eq!(r["status"], "optimal");
        assert_eq!((n(&s, "C1"), n(&s, "C2"), n(&s, "C3"), n(&s, "D1")), (0.0, 1.0, 1.0, 20.0));
    }

    #[test]
    fn grg_finds_smooth_optima() {
        // Largest rectangle with perimeter at most 20: a 5 × 5 square, area 25.
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("A2", "1"), ("B1", "=A1*A2"), ("B2", "=2*A1+2*A2")]);
        let r = s.execute("data.solver", json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20"]})).unwrap();
        assert_eq!(r["status"], "converged", "{r}");
        assert!((n(&s, "A1") - 5.0).abs() < 1e-3 && (n(&s, "A2") - 5.0).abs() < 1e-3, "{r}");
        // Unconstrained minimum of (x − 3)² + (y + 1)² at (3, −1) once negatives are allowed.
        set(&mut s, &[("C1", "0"), ("C2", "0"), ("D1", "=(C1-3)^2+(C2+1)^2")]);
        s.execute("data.solver", json!({"objective": "D1", "goal": "min", "variables": "C1:C2", "nonNegative": false})).unwrap();
        assert!((n(&s, "C1") - 3.0).abs() < 1e-4 && (n(&s, "C2") + 1.0).abs() < 1e-4);
        // Value Of: x³ = 8 → x = 2.
        set(&mut s, &[("E1", "1"), ("F1", "=E1^3")]);
        s.execute("data.solver", json!({"objective": "F1", "goal": "value", "value": 8, "variables": "E1"})).unwrap();
        assert!((n(&s, "E1") - 2.0).abs() < 1e-4);
    }

    #[test]
    fn evolutionary_finds_the_global_minimum_of_non_smooth_and_multimodal_models() {
        let mut s = s();
        // |x − 3| + |y + 2| has a kink at its minimum (3, −2).
        set(&mut s, &[("A1", "0"), ("A2", "0"), ("B1", "=ABS(A1-3)+ABS(A2+2)")]);
        let p =
            json!({"objective": "B1", "goal": "min", "variables": "A1:A2", "constraints": ["A1:A2 >= -10", "A1:A2 <= 10"], "method": "evolutionary"});
        let r = s.execute("data.solver", p.clone()).unwrap();
        assert!(r["status"] == "converged" || r["status"] == "cannotImprove", "{r}");
        assert!((n(&s, "A1") - 3.0).abs() < 1e-6 && (n(&s, "A2") + 2.0).abs() < 1e-6, "{r}");
        // The same seed repeats the run exactly.
        set(&mut s, &[("A1", "0"), ("A2", "0")]);
        let again = s.execute("data.solver", p).unwrap();
        assert_eq!(again["values"], r["values"]);
        assert_eq!(again["evaluations"], r["evaluations"]);
        // Rastrigin's function 20 + Σ (x² − 10 cos 2πx) on [−5.12, 5.12]² has its global minimum 0
        // at the origin among many local minima; GRG from (3.3, −2.7) stops at a local one.
        set(&mut s, &[("C1", "3.3"), ("C2", "-2.7"), ("D1", "=20+C1^2-10*COS(2*PI()*C1)+C2^2-10*COS(2*PI()*C2)")]);
        let base = json!({"objective": "D1", "goal": "min", "variables": "C1:C2", "constraints": ["C1:C2 >= -5.12", "C1:C2 <= 5.12"]});
        s.execute("data.solver", base.clone()).unwrap();
        assert!(n(&s, "D1") > 0.5, "GRG is local: {}", n(&s, "D1"));
        set(&mut s, &[("C1", "3.3"), ("C2", "-2.7")]);
        let mut p = base.clone();
        p["method"] = json!("evolutionary");
        let r = s.execute("data.solver", p).unwrap();
        assert!(n(&s, "D1") < 1e-8 && n(&s, "C1").abs() < 1e-5 && n(&s, "C2").abs() < 1e-5, "{r}");
        // A stepped model: ROUND makes it flat almost everywhere; the minimum is at 7.2.
        set(&mut s, &[("E1", "0"), ("F1", "=ABS(ROUND(E1,0)-7)+ABS(E1-7.2)/100")]);
        s.execute("data.solver", json!({"objective": "F1", "goal": "min", "variables": "E1", "constraints": ["E1 <= 20"], "method": "evolutionary"}))
            .unwrap();
        assert!((n(&s, "E1") - 7.2).abs() < 1e-6, "{}", n(&s, "E1"));
    }

    #[test]
    fn evolutionary_handles_integers_bounds_and_the_saved_model() {
        let mut s = s();
        // The 0/1 knapsack of the branch-and-bound test: items 2 and 3, value 20.
        set(&mut s, &[("D1", "=10*C1+13*C2+7*C3"), ("D2", "=5*C1+6*C2+4*C3")]);
        let p = json!({"objective": "D1", "variables": "C1:C3", "constraints": ["D2 <= 10", "C1:C3 bin"], "method": "evolutionary", "populationSize": 30, "randomSeed": 7});
        let r = s.execute("data.solver", p).unwrap();
        assert!(r["status"] == "converged" || r["status"] == "cannotImprove", "{r}");
        assert_eq!((n(&s, "C1"), n(&s, "C2"), n(&s, "C3"), n(&s, "D1")), (0.0, 1.0, 1.0, 20.0));
        // The method and its options are saved, survive XLSX and solve again.
        let b64 = s.execute("file.saveBytes", json!({"format": "xlsx"})).unwrap()["base64"].as_str().unwrap().to_string();
        s.execute("file.open", json!({"name": "e.xlsx", "base64": b64})).unwrap();
        let m = s.execute("data.solver", json!({})).unwrap();
        assert_eq!(m["model"]["method"], "evolutionary");
        assert_eq!((m["model"]["populationSize"].as_f64(), m["model"]["randomSeed"].as_f64()), (Some(30.0), Some(7.0)));
        set(&mut s, &[("C1", "0"), ("C2", "0"), ("C3", "0")]);
        s.execute("data.solver", json!({"saved": true})).unwrap();
        assert_eq!(n(&s, "D1"), 20.0);
        // Integer constraints: max x + y with x + 2y ≤ 7 on integers in [0, 10] → (7, 0).
        set(&mut s, &[("A1", "0"), ("A2", "0"), ("B1", "=A1+A2"), ("B2", "=A1+2*A2")]);
        let r = s
            .execute(
                "data.solver",
                json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 7", "A1:A2 <= 10", "A1:A2 int"], "method": "evolutionary"}),
            )
            .unwrap();
        assert_eq!((n(&s, "A1"), n(&s, "A2")), (7.0, 0.0), "{r}");
        // Every variable needs bounds unless requireBounds is off; then it searches around the start.
        set(&mut s, &[("E1", "1"), ("F1", "=ABS(E1-4)")]);
        let p = json!({"objective": "F1", "goal": "min", "variables": "E1", "method": "evolutionary"});
        assert!(s.execute("data.solver", p.clone()).is_err());
        let mut p = p;
        p["requireBounds"] = json!(false);
        s.execute("data.solver", p).unwrap();
        assert!((n(&s, "E1") - 4.0).abs() < 1e-6);
        // Infeasible constraints and a tiny budget are reported without changing the cells.
        set(&mut s, &[("E1", "1")]);
        let r = s
            .execute("data.solver", json!({"objective": "F1", "variables": "E1", "constraints": ["E1 <= 3", "F1 >= 10"], "method": "evolutionary"}))
            .unwrap();
        assert_eq!(r["status"], "infeasible", "{r}");
        let r = s
            .execute(
                "data.solver",
                json!({"objective": "F1", "variables": "E1", "constraints": ["E1 <= 3"], "method": "evolutionary", "maxEvaluations": 3}),
            )
            .unwrap();
        assert_eq!(r["status"], "iterationLimit", "{r}");
        assert_eq!(n(&s, "E1"), 1.0);
    }

    /// Text or number at `a1` on sheet `name`.
    fn at(s: &Session, name: &str, a1: &str) -> Value {
        let d = s.doc().unwrap();
        d.wb.sheet(d.wb.sheet_index(name).unwrap()).unwrap().value(CellRef::parse(a1).unwrap())
    }

    /// The row of the first cell holding `text` on sheet `name`.
    fn row_of(s: &Session, name: &str, text: &str) -> u32 {
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(d.wb.sheet_index(name).unwrap()).unwrap();
        (0..60).find(|r| (0..8).any(|c| sh.value(CellRef::new(*r, c)) == Value::text(text))).unwrap()
    }

    #[test]
    fn answer_and_limits_reports_describe_the_wyndor_glass_solution() {
        // Wyndor Glass with labels: doors and windows across, the three plants' hours down.
        let mut s = s();
        s.execute(
            "range.setValues",
            json!({"range": "A1", "values": [[null, "Doors", "Windows", "Used", "Available"], ["Batches", 0, 0, null, null], ["Plant 1", 1, 0, null, 4], ["Plant 2", 0, 2, null, 12], ["Plant 3", 3, 2, null, 18], ["Profit", 3, 5, null, null]]}),
        )
        .unwrap();
        for r in 3..=6 {
            set(&mut s, &[(&format!("D{r}"), &format!("=SUMPRODUCT(B{r}:C{r},$B$2:$C$2)"))]);
        }
        let solved = s
            .execute(
                "data.solver",
                json!({"objective": "D6", "variables": "B2:C2", "constraints": ["D3:D5 <= E3:E5"], "method": "simplex", "reports": ["answer", "limits"]}),
            )
            .unwrap();
        assert_eq!(solved["status"], "optimal", "{solved}");
        assert_eq!(solved["reports"], json!(["Answer Report 1", "Limits Report 1"]));
        assert_eq!(solved["original"], json!({"B2": 0.0, "C2": 0.0}));
        let d = s.doc().unwrap();
        let names: Vec<&str> = d.wb.sheets.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Answer Report 1", "Limits Report 1", "Sheet1"]);
        assert_eq!(d.wb.active().unwrap().name, "Sheet1", "the model's sheet stays active");
        assert_eq!(saved(&s), "solver_opt", "the saved model follows its sheet");
        let a = "Answer Report 1";
        assert_eq!(at(&s, a, "A1"), Value::text("GridCraft Answer Report"));
        // Objective: $D$6 "Profit Used", from 0 to 36.
        let r = row_of(&s, a, "$D$6");
        assert_eq!(at(&s, a, &format!("C{}", r + 1)), Value::text("Profit Used"));
        assert_eq!((at(&s, a, &format!("D{}", r + 1)), at(&s, a, &format!("E{}", r + 1))), (Value::number(0.0), Value::number(36.0)));
        // Variables: doors 0 → 2, windows 0 → 6, continuous.
        let r = row_of(&s, a, "$B$2") + 1;
        assert_eq!(at(&s, a, &format!("C{r}")), Value::text("Batches Doors"));
        assert_eq!(at(&s, a, &format!("E{r}")), Value::number(2.0));
        assert_eq!(at(&s, a, &format!("F{r}")), Value::text("Contin"));
        assert_eq!(at(&s, a, &format!("E{}", r + 1)), Value::number(6.0));
        // Constraints: plant 1 has 2 hours to spare; plants 2 and 3 are binding.
        let r = row_of(&s, a, "$D$3") + 1;
        assert_eq!(at(&s, a, &format!("E{r}")), Value::text("$D$3<=$E$3"));
        assert_eq!((at(&s, a, &format!("F{r}")), at(&s, a, &format!("G{r}"))), (Value::text("Not Binding"), Value::number(2.0)));
        for k in 1..=2 {
            assert_eq!((at(&s, a, &format!("F{}", r + k)), at(&s, a, &format!("G{}", r + k))), (Value::text("Binding"), Value::number(0.0)));
        }
        // Limits with the other variable held: doors 0..2 (profit 30..36), windows 0..6 (6..36).
        let l = "Limits Report 1";
        let r = row_of(&s, l, "$B$2") + 1;
        let row = |r: u32| ["D", "E", "F", "G", "H"].map(|c| at(&s, l, &format!("{c}{r}")).as_f64().unwrap());
        assert_eq!(row(r), [2.0, 0.0, 30.0, 2.0, 36.0]);
        assert_eq!(row(r + 1), [6.0, 0.0, 6.0, 6.0, 36.0]);
        // data.solverReport writes reports from the saved model and a given solution.
        let out =
            s.execute("data.solverReport", json!({"reports": ["answer"], "values": {"B2": 2, "C2": 6}, "original": {"B2": 1, "C2": 1}})).unwrap();
        assert_eq!(out["sheets"], json!(["Answer Report 2"]));
        let r = row_of(&s, "Answer Report 2", "$D$6") + 1;
        assert_eq!(at(&s, "Answer Report 2", &format!("D{r}")), Value::number(8.0));
        // Undo removes the reports and keeps the solution; refused requests write nothing.
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.doc().unwrap().wb.sheets.len(), 3);
        for p in [
            json!({}),
            json!({"reports": []}),
            json!({"reports": "answer"}),
            json!({"reports": ["population"]}),
            json!({"reports": ["answer"], "values": {"Z9": 1}}),
            json!({"reports": ["answer"], "values": {"B2": "x"}}),
            json!({"reports": ["limits"], "constraints": ["D3:D5 <= E3:E5", "B2:C2 int"]}),
            json!({"reports": ["limits"], "method": "evolutionary", "constraints": ["D3:D5 <= E3:E5", "B2:C2 <= 10"]}),
        ] {
            assert!(s.execute("data.solverReport", p.clone()).is_err(), "{p}");
        }
        assert_eq!(s.doc().unwrap().wb.sheets.len(), 3);
        // With the solve, its reports and the saved model undone (one step), the result still
        // describes the solution.
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!((s.doc().unwrap().wb.sheets.len(), n(&s, "B2")), (1, 0.0));
        let mut p = solved.clone();
        p["reports"] = json!(["answer"]);
        assert_eq!(s.execute("data.solverReport", p).unwrap()["sheets"], json!(["Answer Report 1"]));
        let r = row_of(&s, "Answer Report 1", "$B$2") + 1;
        assert_eq!(at(&s, "Answer Report 1", &format!("E{r}")), Value::number(2.0));
    }

    /// The numbers in columns `cols` of the row below the cell holding `text`.
    fn row_after(s: &Session, sheet: &str, text: &str, cols: &[&str]) -> Vec<f64> {
        let r = row_of(s, sheet, text) + 1;
        cols.iter().map(|c| at(s, sheet, &format!("{c}{r}")).as_f64().unwrap_or(f64::NAN)).collect()
    }

    fn close(got: &[f64], want: &[f64]) -> bool {
        got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() <= 1e-6 * (1.0 + w.abs()))
    }

    #[test]
    fn sensitivity_report_of_the_wyndor_glass_lp() {
        // Hillier & Lieberman's Wyndor Glass sensitivity analysis: shadow prices 0, 1.5 and 1;
        // the profit per door may range over 0..7.5 and per window over 2..∞; plant 1's hours
        // over 2..∞ and plants 2 and 3's within ±6.
        let mut s = s();
        s.execute(
            "range.setValues",
            json!({"range": "A1", "values": [[null, "Doors", "Windows", "Used", "Available"], ["Batches", 0, 0, null, null], ["Plant 1", 1, 0, null, 4], ["Plant 2", 0, 2, null, 12], ["Plant 3", 3, 2, null, 18], ["Profit", 3, 5, null, null]]}),
        )
        .unwrap();
        for r in 3..=6 {
            set(&mut s, &[(&format!("D{r}"), &format!("=SUMPRODUCT(B{r}:C{r},$B$2:$C$2)"))]);
        }
        let solved = s
            .execute(
                "data.solver",
                json!({"objective": "D6", "variables": "B2:C2", "constraints": ["D3:D5 <= E3:E5"], "method": "simplex", "reports": ["answer", "sensitivity"]}),
            )
            .unwrap();
        assert_eq!(solved["reports"], json!(["Answer Report 1", "Sensitivity Report 1"]), "{solved}");
        let r = "Sensitivity Report 1";
        assert_eq!(at(&s, r, "A1"), Value::text("GridCraft Sensitivity Report"));
        let h = row_of(&s, r, "Reduced Cost") + 1;
        for (c, head) in [("B", "Cell"), ("F", "Objective Coefficient"), ("G", "Allowable Increase")] {
            assert_eq!(at(&s, r, &format!("{c}{h}")), Value::text(head));
        }
        row_of(&s, r, "Shadow Price");
        let cols = ["D", "E", "F", "G", "H"];
        // Final value, reduced cost, objective coefficient, allowable increase and decrease.
        assert!(close(&row_after(&s, r, "$B$2", &cols), &[2.0, 0.0, 3.0, 4.5, 3.0]), "{:?}", row_after(&s, r, "$B$2", &cols));
        assert!(close(&row_after(&s, r, "$C$2", &cols), &[6.0, 0.0, 5.0, 1e30, 3.0]), "{:?}", row_after(&s, r, "$C$2", &cols));
        assert_eq!(at(&s, r, &format!("C{}", row_of(&s, r, "$C$2") + 1)), Value::text("Batches Windows"));
        // Final value, shadow price, right side, allowable increase and decrease.
        assert!(close(&row_after(&s, r, "$D$3", &cols), &[2.0, 0.0, 4.0, 1e30, 2.0]), "{:?}", row_after(&s, r, "$D$3", &cols));
        assert!(close(&row_after(&s, r, "$D$4", &cols), &[12.0, 1.5, 12.0, 6.0, 6.0]), "{:?}", row_after(&s, r, "$D$4", &cols));
        assert!(close(&row_after(&s, r, "$D$5", &cols), &[18.0, 1.0, 18.0, 6.0, 6.0]), "{:?}", row_after(&s, r, "$D$5", &cols));
        // Minimising the negated profit gives the same prices with the objective's sign.
        set(&mut s, &[("F6", "=-D6"), ("B2", "0"), ("C2", "0")]);
        let out = s
            .execute(
                "data.solver",
                json!({"objective": "F6", "goal": "min", "variables": "B2:C2", "constraints": ["D3:D5 <= E3:E5"], "method": "simplex", "reports": ["sensitivity"]}),
            )
            .unwrap();
        let r = out["reports"][0].as_str().unwrap().to_string();
        assert!(close(&row_after(&s, &r, "$B$2", &cols), &[2.0, 0.0, -3.0, 3.0, 4.5]), "{:?}", row_after(&s, &r, "$B$2", &cols));
        assert!(close(&row_after(&s, &r, "$D$4", &cols), &[12.0, -1.5, 12.0, 6.0, 6.0]), "{:?}", row_after(&s, &r, "$D$4", &cols));
    }

    #[test]
    fn sensitivity_report_shows_bounds_as_reduced_costs() {
        // max 3x + 5y with y ≤ 4 (a bound on the variable) and x + y ≤ 6: x = 2, y = 4. Bounds
        // aren't listed as constraints; y's bound shows as its reduced cost 5 − 3 = 2.
        let mut s = s();
        set(&mut s, &[("A1", "0"), ("A2", "0"), ("B1", "=3*A1+5*A2"), ("B2", "=A1+A2")]);
        let out = s
            .execute(
                "data.solver",
                json!({"objective": "B1", "variables": "A1:A2", "constraints": ["A2 <= 4", "B2 <= 6"], "method": "simplex", "reports": ["sensitivity"]}),
            )
            .unwrap();
        assert_eq!(out["objective"], 26.0, "{out}");
        let r = "Sensitivity Report 1";
        let cols = ["D", "E", "F"];
        assert!(close(&row_after(&s, r, "$A$1", &cols), &[2.0, 0.0, 3.0]), "{:?}", row_after(&s, r, "$A$1", &cols));
        assert!(close(&row_after(&s, r, "$A$2", &cols), &[4.0, 2.0, 5.0]), "{:?}", row_after(&s, r, "$A$2", &cols));
        assert!(close(&row_after(&s, r, "$B$2", &cols), &[6.0, 3.0, 6.0]), "{:?}", row_after(&s, r, "$B$2", &cols));
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(d.wb.sheet_index(r).unwrap()).unwrap();
        let listed = (0..40).filter(|row| sh.value(CellRef::new(*row, 1)) == Value::text("$A$2")).count();
        assert_eq!(listed, 1, "the bound A2 <= 4 isn't a constraint row");
    }

    #[test]
    fn sensitivity_report_of_a_nonlinear_model() {
        // The largest rectangle with perimeter at most 20 (5 × 5): another unit of perimeter
        // adds 2.5 to the area. With the width held at most 2, the height is 8, the perimeter's
        // multiplier is 1 (∂area/∂height = 2 over its coefficient 2) and the width's bound shows
        // as its reduced gradient 8 − 1·2 = 6.
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("A2", "1"), ("B1", "=A1*A2"), ("B2", "=2*A1+2*A2")]);
        let out = s
            .execute("data.solver", json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20"], "reports": ["sensitivity"]}))
            .unwrap();
        let r = out["reports"][0].as_str().unwrap().to_string();
        row_of(&s, &r, "Reduced Gradient");
        row_of(&s, &r, "Lagrange Multiplier");
        let near = |got: Vec<f64>, want: &[f64]| got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3) || panic!("{got:?} vs {want:?}");
        near(row_after(&s, &r, "$A$1", &["D", "E"]), &[5.0, 0.0]);
        near(row_after(&s, &r, "$B$2", &["D", "E"]), &[20.0, 2.5]);
        set(&mut s, &[("A1", "1"), ("A2", "1")]);
        let out = s
            .execute(
                "data.solver",
                json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20", "A1 <= 2"], "reports": ["sensitivity"]}),
            )
            .unwrap();
        let r = out["reports"][0].as_str().unwrap().to_string();
        near(row_after(&s, &r, "$A$1", &["D", "E"]), &[2.0, 6.0]);
        near(row_after(&s, &r, "$A$2", &["D", "E"]), &[8.0, 0.0]);
        near(row_after(&s, &r, "$B$2", &["D", "E"]), &[20.0, 1.0]);
        // Not for integer models or Evolutionary, unless integers are ignored.
        let base = json!({"reports": ["sensitivity"], "objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20", "A1:A2 int"]});
        assert!(s.execute("data.solverReport", base.clone()).is_err());
        let mut p = base.clone();
        p["constraints"] = json!(["B2 <= 20", "A1:A2 <= 10"]);
        p["method"] = json!("evolutionary");
        assert!(s.execute("data.solverReport", p).is_err());
        let mut p = base;
        p["ignoreIntegers"] = json!(true);
        assert!(s.execute("data.solverReport", p).is_ok());
    }

    #[test]
    fn solver_options_are_saved_with_the_model_and_survive_xlsx() {
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("A2", "1"), ("B1", "=A1*A2"), ("B2", "=2*A1+2*A2")]);
        let p = json!({
            "objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20"],
            "precision": 0.0001, "scaling": false, "ignoreIntegers": true, "integerOptimality": 5,
            "maxEvaluations": 30000, "maxSubproblems": 50, "maxFeasibleSolutions": 3,
            "convergence": 0.001, "derivatives": "forward", "populationSize": 40, "randomSeed": 9,
        });
        let r = s.execute("data.solver", p).unwrap();
        assert_eq!(r["status"], "converged", "{r}");
        assert!((n(&s, "B1") - 25.0).abs() < 1e-2, "forward differences, no scaling: {}", n(&s, "B1"));
        let b64 = s.execute("file.saveBytes", json!({"format": "xlsx"})).unwrap()["base64"].as_str().unwrap().to_string();
        s.execute("file.open", json!({"name": "o.xlsx", "base64": b64})).unwrap();
        let m = s.execute("data.solver", json!({})).unwrap()["model"].clone();
        let want = json!({
            "precision": 0.0001, "scaling": false, "ignoreIntegers": true, "integerOptimality": 5.0,
            "maxEvaluations": 30000.0, "maxSubproblems": 50.0, "maxFeasibleSolutions": 3.0,
            "convergence": 0.001, "derivatives": "forward", "multistart": false, "populationSize": 40.0, "randomSeed": 9.0,
        });
        for (k, v) in want.as_object().unwrap() {
            let got = &m[k];
            let same = match (got.as_f64(), v.as_f64()) {
                (Some(a), Some(b)) => (a - b).abs() <= 1e-12 * b.abs().max(1.0),
                _ => got == v,
            };
            assert!(same, "{k}: {got} vs {v}");
        }
        // The defaults round-trip too, and a solve's result carries its options.
        let r = s.execute("data.solver", json!({"saved": true, "derivatives": "central"})).unwrap();
        assert_eq!(r["model"]["derivatives"], "central");
        assert_eq!(r["model"]["maxSubproblems"], 50);
        assert_eq!(super::option_defaults()["maxSubproblems"], Json::Null);
        // Options as Excel saves them: Iterations (solver_itr) aren't evaluations and stay out of
        // GridCraft's budget; "no limit" is a huge number; tolerances are fractions.
        for (name, v) in [("solver_itr", "100"), ("solver_tol", "0.01"), ("solver_drv", "1"), ("solver_nod", "2147483647"), ("solver_gc_evals", "")] {
            let wb = std::sync::Arc::make_mut(&mut s.doc_mut().unwrap().wb);
            wb.names.retain(|n| !n.name.eq_ignore_ascii_case(name));
            if !v.is_empty() {
                wb.names.push(gridcraft_model::DefinedName {
                    name: name.into(),
                    scope: Some(0),
                    formula: v.into(),
                    comment: String::new(),
                    hidden: true,
                });
            }
        }
        let m = s.execute("data.solver", json!({})).unwrap()["model"].clone();
        assert_eq!((m.get("maxEvaluations"), m.get("maxSubproblems")), (None, None), "{m}");
        assert_eq!((m["integerOptimality"].as_f64(), m["derivatives"].as_str()), (Some(1.0), Some("forward")));
    }

    #[test]
    fn integer_options_relax_and_limit_branch_and_bound() {
        // The 0/1 knapsack: 20 with integers; Ignore Integer Constraints solves the relaxation
        // (item 2 whole and 4/5 of item 1: 21) and keeps binary variables within 0..1.
        let mut s = s();
        set(&mut s, &[("D1", "=10*C1+13*C2+7*C3"), ("D2", "=5*C1+6*C2+4*C3")]);
        let base = json!({"objective": "D1", "variables": "C1:C3", "constraints": ["D2 <= 10", "C1:C3 bin"], "method": "simplex"});
        let mut p = base.clone();
        p["ignoreIntegers"] = json!(true);
        let r = s.execute("data.solver", p).unwrap();
        assert!((r["objective"].as_f64().unwrap() - 21.0).abs() < 1e-9, "{r}");
        assert!((0..3).all(|i| (0.0..=1.0).contains(&n(&s, ["C1", "C2", "C3"][i]))));
        // One subproblem is the relaxation: no integer solution yet.
        set(&mut s, &[("C1", "0"), ("C2", "0"), ("C3", "0")]);
        let mut p = base.clone();
        p["maxSubproblems"] = json!(1);
        let r = s.execute("data.solver", p).unwrap();
        assert_eq!(r["status"], "iterationLimit", "{r}");
        assert!(r["message"].as_str().unwrap().contains("Max Subproblems"), "{r}");
        assert_eq!(n(&s, "D1"), 0.0, "nothing to keep");
        // Stopping at the first integer solution keeps it.
        let mut p = base.clone();
        p["maxFeasibleSolutions"] = json!(1);
        let r = s.execute("data.solver", p).unwrap();
        assert!(r["status"] == "iterationLimit" || r["status"] == "optimal", "{r}");
        if r["status"] == "iterationLimit" {
            assert!(r["message"].as_str().unwrap().contains("Max Feasible Solutions"), "{r}");
        }
        assert!(n(&s, "D2") <= 10.0 && [n(&s, "C1"), n(&s, "C2"), n(&s, "C3")].iter().all(|v| *v == 0.0 || *v == 1.0));
    }

    #[test]
    fn multistart_finds_the_global_minimum_grg_misses() {
        // (x² − 4)² + x + (y² − 1)² + y/2 has four local minima near (±2, ±1); the global one is
        // near (−2.03, −1.06). GRG from (3, 2) stops at the nearest; Multistart finds the global.
        let mut s = s();
        set(&mut s, &[("C1", "3"), ("C2", "2"), ("D1", "=(C1^2-4)^2+C1+(C2^2-1)^2+C2/2")]);
        let base = json!({"objective": "D1", "goal": "min", "variables": "C1:C2", "constraints": ["C1:C2 >= -5", "C1:C2 <= 5"]});
        s.execute("data.solver", base.clone()).unwrap();
        assert!(n(&s, "C1") > 0.0 && n(&s, "C2") > 0.0, "GRG is local");
        set(&mut s, &[("C1", "3"), ("C2", "2")]);
        let mut p = base.clone();
        p["multistart"] = json!(true);
        p["populationSize"] = json!(20);
        let r = s.execute("data.solver", p.clone()).unwrap();
        assert!((n(&s, "C1") + 2.03).abs() < 0.01 && (n(&s, "C2") + 1.06).abs() < 0.01, "{r}");
        assert!(r["message"].as_str().unwrap().starts_with("Multistart"), "{r}");
        // The same seed repeats the run; without bounds it needs requireBounds: false.
        set(&mut s, &[("C1", "3"), ("C2", "2")]);
        assert_eq!(s.execute("data.solver", p.clone()).unwrap()["values"], r["values"]);
        set(&mut s, &[("E1", "1"), ("F1", "=(E1-4)^2")]);
        let p = json!({"objective": "F1", "goal": "min", "variables": "E1", "multistart": true});
        assert!(s.execute("data.solver", p.clone()).is_err());
        let mut p = p;
        p["requireBounds"] = json!(false);
        s.execute("data.solver", p).unwrap();
        assert!((n(&s, "E1") - 4.0).abs() < 1e-3);
    }

    #[test]
    fn limits_report_of_a_nonlinear_model() {
        // The largest rectangle with perimeter at most 20 (5 × 5): with the height held at 5, the
        // width ranges over 0..5, the area over 0..25.
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("A2", "1"), ("B1", "=A1*A2"), ("B2", "=2*A1+2*A2")]);
        let r =
            s.execute("data.solver", json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20"], "reports": ["limits"]})).unwrap();
        assert_eq!(r["reports"], json!(["Limits Report 1"]), "{r}");
        let l = "Limits Report 1";
        let r = row_of(&s, l, "$A$1") + 1;
        let got: Vec<f64> = ["E", "F", "G", "H"].iter().map(|c| at(&s, l, &format!("{c}{r}")).as_f64().unwrap()).collect();
        for (g, want) in got.iter().zip([0.0, 0.0, 5.0, 25.0]) {
            assert!((g - want).abs() < 1e-3, "{got:?}");
        }
    }

    #[test]
    fn infeasible_unbounded_and_nonlinear_models_are_reported() {
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("B1", "=2*A1")]);
        for method in ["simplex", "grg"] {
            let r = s
                .execute("data.solver", json!({"objective": "B1", "variables": "A1", "constraints": ["A1 >= 5", "B1 <= 6"], "method": method}))
                .unwrap();
            assert_eq!(r["status"], "infeasible", "{method}: {r}");
            assert_eq!(r["changed"], false);
            assert_eq!(n(&s, "A1"), 1.0, "an infeasible model leaves the cells alone");
        }
        let r = s.execute("data.solver", json!({"objective": "B1", "variables": "A1", "method": "simplex"})).unwrap();
        assert_eq!(r["status"], "unbounded");
        let r = s.execute("data.solver", json!({"objective": "B1", "variables": "A1", "method": "grg", "maxEvaluations": 5000})).unwrap();
        assert!(r["status"] == "unbounded" || r["status"] == "iterationLimit", "{r}");
        set(&mut s, &[("A1", "1"), ("C1", "=A1^2")]);
        let r = s.execute("data.solver", json!({"objective": "C1", "goal": "min", "variables": "A1", "method": "simplex"})).unwrap();
        assert_eq!(r["status"], "notLinear");
    }

    #[test]
    fn hostile_models_are_refused_without_panicking() {
        let mut s = s();
        let r = s.execute("data.solver", json!({})).unwrap();
        assert!(r["model"].is_null());
        assert!(s.take_ui_requests().iter().any(|q| matches!(q, crate::UiRequest::Dialog(n, _) if n == "solver")));
        set(&mut s, &[("B1", "=SUM(A1:A1000)"), ("C1", "5")]);
        for p in [
            json!({"objective": "B1", "variables": "A1:A1000"}),
            json!({"objective": "B1", "variables": "A1:XFD1048576"}),
            json!({"objective": "C1", "variables": "A1"}),
            json!({"objective": "B1:B2", "variables": "A1"}),
            json!({"objective": "B1", "variables": "B1"}),
            json!({"objective": "B1", "variables": "A1", "goal": "sideways"}),
            json!({"objective": "B1", "variables": "A1", "goal": "value"}),
            json!({"objective": "B1", "variables": "A1", "method": "evolutionary"}),
            json!({"objective": "B1", "variables": "A1", "method": "annealing"}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["A1 ?? 3"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": "A1 <= 3"}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["A1:A3 <= C1:C2"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["C1 int"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["A1:A1048576 <= 1"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": vec![json!("A1 <= 1"); 1001]}),
            json!({"objective": "Nope!B1", "variables": "A1"}),
            json!({"objective": "B1", "variables": "A1", "derivatives": "sideways"}),
        ] {
            assert!(s.execute("data.solver", p.clone()).is_err(), "{p}");
        }
        // Huge evaluation requests are capped, and a model erroring everywhere doesn't panic.
        set(&mut s, &[("D1", "=1/0+A1")]);
        let r = s.execute("data.solver", json!({"objective": "D1", "variables": "A1", "maxEvaluations": 1e300})).unwrap();
        assert_ne!(r["status"], "converged");
    }
}
